//! Pins every struct-version constant the wallet file layout depends on.
//!
//! A failing assertion here is not a bug to patch quietly: it means a layout changed. That is a
//! deliberate version bump, and it needs fresh pinned-release vectors under `tests/vectors/`
//! covering both the old and the new layout before this test is updated to match.

use zcash_keys::keys::UnifiedFullViewingKey;

use pepper_sync::config::{PerformanceLevel, SyncConfig};
use pepper_sync::wallet::{
    IronwoodNote, NullifierMap, OrchardNote, OutgoingIronwoodNote, OutgoingOrchardNote,
    OutgoingSaplingNote, SaplingNote, ScanTarget, ShardTrees, SyncState, TransparentCoin,
    TreeBounds, WalletBlock, WalletTransaction,
};
use zingolib_common::chain::ChainType;
use zingolib_common::keys::{ReceiverSelection, UnifiedKeyStore};
use zingolib_common::serialization::ReadableWriteable;
use zingolib_common::status::ConfirmationStatus;
use zingolib_file_format::WalletFile;
use zingolib_price::PriceList;

#[test]
fn struct_versions_are_pinned() {
    // pepper-sync
    assert_eq!(
        <ScanTarget as ReadableWriteable>::VERSION,
        0,
        "ScanTarget::VERSION"
    );
    assert_eq!(
        <SyncState as ReadableWriteable>::VERSION,
        4,
        "SyncState::VERSION"
    );
    assert_eq!(
        <TreeBounds as ReadableWriteable>::VERSION,
        1,
        "TreeBounds::VERSION"
    );
    assert_eq!(
        <NullifierMap as ReadableWriteable>::VERSION,
        2,
        "NullifierMap::VERSION"
    );
    assert_eq!(
        <WalletBlock as ReadableWriteable>::VERSION,
        0,
        "WalletBlock::VERSION"
    );
    assert_eq!(
        <WalletTransaction as ReadableWriteable<&ChainType, &ChainType>>::VERSION,
        1,
        "WalletTransaction::VERSION"
    );
    assert_eq!(
        <TransparentCoin as ReadableWriteable>::VERSION,
        1,
        "TransparentCoin::VERSION"
    );
    assert_eq!(
        <SaplingNote as ReadableWriteable>::VERSION,
        2,
        "SaplingNote::VERSION"
    );
    assert_eq!(
        <OrchardNote as ReadableWriteable>::VERSION,
        2,
        "OrchardNote::VERSION"
    );
    assert_eq!(
        <IronwoodNote as ReadableWriteable>::VERSION,
        2,
        "IronwoodNote::VERSION"
    );
    assert_eq!(
        <OutgoingSaplingNote as ReadableWriteable<&ChainType, &ChainType>>::VERSION,
        1,
        "OutgoingSaplingNote::VERSION"
    );
    assert_eq!(
        <OutgoingOrchardNote as ReadableWriteable<&ChainType, &ChainType>>::VERSION,
        1,
        "OutgoingOrchardNote::VERSION"
    );
    assert_eq!(
        <OutgoingIronwoodNote as ReadableWriteable<&ChainType, &ChainType>>::VERSION,
        1,
        "OutgoingIronwoodNote::VERSION"
    );
    assert_eq!(
        <ShardTrees as ReadableWriteable>::VERSION,
        1,
        "ShardTrees::VERSION"
    );
    assert_eq!(
        <PerformanceLevel as ReadableWriteable>::VERSION,
        0,
        "PerformanceLevel::VERSION"
    );
    assert_eq!(
        <SyncConfig as ReadableWriteable>::VERSION,
        2,
        "SyncConfig::VERSION"
    );

    // zingolib-common
    assert_eq!(
        <ConfirmationStatus as ReadableWriteable>::VERSION,
        1,
        "ConfirmationStatus::VERSION"
    );
    assert_eq!(
        <UnifiedKeyStore as ReadableWriteable<ChainType, ChainType>>::VERSION,
        0,
        "UnifiedKeyStore::VERSION"
    );
    assert_eq!(
        <UnifiedFullViewingKey as ReadableWriteable<ChainType, ChainType>>::VERSION,
        0,
        "UnifiedFullViewingKey::VERSION"
    );
    assert_eq!(
        <ReceiverSelection as ReadableWriteable>::VERSION,
        2,
        "ReceiverSelection::VERSION"
    );

    // zingolib-price
    assert_eq!(
        <PriceList as ReadableWriteable>::VERSION,
        0,
        "PriceList::VERSION"
    );

    // zingolib-file-format
    assert_eq!(WalletFile::VERSION, 41, "WalletFile::VERSION");
}
