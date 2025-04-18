/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::{sync::Arc, time::Instant};

use ahash::AHashMap;
use directory::Permission;
use email::message::{
    cache::{MessageCache, MessageCacheAccess},
    delete::EmailDeletion,
    metadata::MessageData,
};
use imap_proto::{
    Command, ResponseCode, ResponseType, StatusResponse,
    parser::parse_sequence_set,
    receiver::{Request, Token},
};
use trc::AddContext;

use crate::core::{ImapId, SavedSearch, SelectedMailbox, Session, SessionData};
use common::{listener::SessionStream, storage::index::ObjectIndexBuilder};
use jmap_proto::types::{acl::Acl, collection::Collection, keyword::Keyword};
use store::{roaring::RoaringBitmap, write::BatchBuilder};

use super::{ImapContext, ToModSeq};

impl<T: SessionStream> Session<T> {
    pub async fn handle_expunge(
        &mut self,
        request: Request<Command>,
        is_uid: bool,
    ) -> trc::Result<()> {
        // Validate access
        self.assert_has_permission(Permission::ImapExpunge)?;

        let op_start = Instant::now();
        let (data, mailbox) = self.state.select_data();

        // Validate ACL
        if !data
            .check_mailbox_acl(
                mailbox.id.account_id,
                mailbox.id.mailbox_id,
                Acl::RemoveItems,
            )
            .await
            .imap_ctx(&request.tag, trc::location!())?
        {
            return Err(trc::ImapEvent::Error
                .into_err()
                .details(concat!(
                    "You do not have the required permissions ",
                    "to remove messages from this mailbox."
                ))
                .code(ResponseCode::NoPerm)
                .id(request.tag));
        }

        // Parse sequence to operate on
        let sequence = match request.tokens.into_iter().next() {
            Some(Token::Argument(value)) if is_uid => {
                let sequence = parse_sequence_set(&value).map_err(|err| {
                    trc::ImapEvent::Error
                        .into_err()
                        .details(err)
                        .ctx(trc::Key::Type, ResponseType::Bad)
                        .id(request.tag.clone())
                })?;
                Some(
                    mailbox
                        .sequence_to_ids(&sequence, true)
                        .await
                        .map_err(|err| err.id(request.tag.clone()))?,
                )
            }

            _ => None,
        };

        // Expunge
        data.expunge(mailbox.clone(), sequence, op_start)
            .await
            .imap_ctx(&request.tag, trc::location!())?;

        // Clear saved searches
        *mailbox.saved_search.lock() = SavedSearch::None;

        // Synchronize messages
        let modseq = data
            .write_mailbox_changes(&mailbox, self.is_qresync)
            .await
            .imap_ctx(&request.tag, trc::location!())?;
        let mut response =
            StatusResponse::completed(Command::Expunge(is_uid)).with_tag(request.tag);

        if self.is_condstore {
            response = response.with_code(ResponseCode::HighestModseq {
                modseq: modseq.to_modseq(),
            });
        }

        self.write_bytes(response.into_bytes()).await
    }
}

impl<T: SessionStream> SessionData<T> {
    pub async fn expunge(
        &self,
        mailbox: Arc<SelectedMailbox>,
        sequence: Option<AHashMap<u32, ImapId>>,
        op_start: Instant,
    ) -> trc::Result<()> {
        // Obtain message ids
        let account_id = mailbox.id.account_id;
        let mut deleted_ids = RoaringBitmap::from_iter(
            self.server
                .get_cached_messages(account_id)
                .await
                .caused_by(trc::location!())?
                .in_mailbox_with_keyword(mailbox.id.mailbox_id, &Keyword::Deleted)
                .map(|(id, _)| id),
        );

        // Filter by sequence
        if let Some(sequence) = &sequence {
            deleted_ids &= RoaringBitmap::from_iter(sequence.keys());
        }

        // Delete ids
        let mut batch = BatchBuilder::new();
        self.email_untag_or_delete(account_id, mailbox.id.mailbox_id, &deleted_ids, &mut batch)
            .await
            .caused_by(trc::location!())?;

        trc::event!(
            Imap(trc::ImapEvent::Expunge),
            SpanId = self.session_id,
            AccountId = account_id,
            MailboxId = mailbox.id.mailbox_id,
            DocumentId = deleted_ids.iter().map(trc::Value::from).collect::<Vec<_>>(),
            Elapsed = op_start.elapsed()
        );

        // Write changes on source account
        if !batch.is_empty() {
            self.server
                .commit_batch(batch)
                .await
                .caused_by(trc::location!())?;
        }

        Ok(())
    }

    pub async fn email_untag_or_delete(
        &self,
        account_id: u32,
        mailbox_id: u32,
        deleted_ids: &RoaringBitmap,
        batch: &mut BatchBuilder,
    ) -> trc::Result<()> {
        let mut destroy_ids = RoaringBitmap::new();
        batch
            .with_account_id(account_id)
            .with_collection(Collection::Email);

        self.server
            .get_archives(account_id, Collection::Email, deleted_ids, |id, data_| {
                let data = data_
                    .to_unarchived::<MessageData>()
                    .caused_by(trc::location!())?;

                if !data.inner.has_mailbox_id(mailbox_id) {
                    return Ok(true);
                } else if data.inner.mailboxes.len() == 1 {
                    destroy_ids.insert(id);
                    return Ok(true);
                }

                // Untag message from this mailbox and remove Deleted flag
                let mut new_data = data.deserialize().caused_by(trc::location!())?;
                new_data.change_id = batch.change_id();
                new_data.remove_mailbox(mailbox_id);
                new_data.remove_keyword(&Keyword::Deleted);

                // Write changes
                batch
                    .update_document(id)
                    .custom(
                        ObjectIndexBuilder::new()
                            .with_current(data)
                            .with_changes(new_data),
                    )
                    .caused_by(trc::location!())?
                    .commit_point();

                Ok(true)
            })
            .await
            .caused_by(trc::location!())?;

        if !destroy_ids.is_empty() {
            // Delete message from all mailboxes
            self.server
                .emails_tombstone(account_id, batch, destroy_ids)
                .await
                .caused_by(trc::location!())?;
        }

        Ok(())
    }
}
