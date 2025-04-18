/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use common::storage::index::{IndexValue, IndexableAndSerializableObject, IndexableObject};

use super::{ArchivedIdentity, Identity};

impl IndexableObject for Identity {
    fn index_values(&self) -> impl Iterator<Item = IndexValue<'_>> {
        [IndexValue::LogChild { prefix: None }].into_iter()
    }
}

impl IndexableObject for &ArchivedIdentity {
    fn index_values(&self) -> impl Iterator<Item = IndexValue<'_>> {
        [IndexValue::LogChild { prefix: None }].into_iter()
    }
}

impl IndexableAndSerializableObject for Identity {}
