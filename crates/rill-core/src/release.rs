//! Where the signer binary comes from, named once.
//!
//! An install line a stranger can run is three strings glued together: a GitHub origin, an asset
//! filename, and the name the downloaded file is given locally. Each of those used to be typed out
//! wherever it was needed, and they disagreed. The release workflow's matrix held one set of
//! filenames and its publish list another; the README held a third copy; and the generated
//! instructions in the reference implementation pointed at `eseslabs/rill`, which redirects to the
//! TypeScript specification and has never published a binary built from this source. A reader who
//! followed that line downloaded an error page and made it executable.
//!
//! U5 resolved the origin to this repository's only remote, because a tag pushed here can put an
//! asset nowhere else. This module is where that answer lives, so the document the server hands an
//! agent and the workflow that produces the file it names cannot drift apart: both read these
//! constants, and a rename is one edit made on purpose.
//!
//! Strings only. Assembling a line is the caller's job, which keeps this crate free of formatting
//! decisions it has no business making, and free of I/O it must never gain.

/// The origin, without a scheme, for prose that names it rather than links to it.
pub const RELEASE_REPO: &str = "github.com/rifuki/rill";

/// The releases page a reader opens to see what exists.
pub const RELEASES_URL: &str = "https://github.com/rifuki/rill/releases";

/// The directory every published asset hangs under. `latest` rather than a pinned tag: the README
/// and the generated instructions both have to keep working across releases without being edited,
/// and the checksum beside each asset is what makes an unpinned download checkable.
pub const LATEST_DOWNLOAD_URL: &str = "https://github.com/rifuki/rill/releases/latest/download";

/// The name the signer ships under and answers to: the first line `--status` prints, the name the
/// release smoke step greps for, and the filename every instruction tells a reader to keep.
///
/// `rill_cli::BINARY_NAME` is this constant, not a copy of it. Four surfaces claim the name, and a
/// rename that reached three of them produced a release whose binary answered to something nobody
/// had downloaded.
pub const WALLET_BINARY: &str = "rill-wallet";

/// What a checksum file is called, given its asset's name.
pub const CHECKSUM_SUFFIX: &str = ".sha256";

/// One release asset, with the platform it is for.
///
/// The pairing is the part worth holding in one place. Two independent lists, one of filenames and
/// one of platform labels, is how a document comes to tell a Linux reader to download the macOS
/// build: both lists are individually correct and the order between them is not checked by
/// anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalletAsset {
    /// How a reader recognises their own machine, in a document's own words.
    pub platform: &'static str,
    /// The published filename, byte for byte.
    pub file: &'static str,
}

/// Every asset a release publishes, in the order the workflow's matrix builds them.
///
/// These names are a contract with every install instruction already in the wild, so a change here
/// renames a file strangers are being told to download. `bins/rill/tests/release_contract.rs`
/// checks the workflow and the README against this list.
pub const WALLET_ASSETS: [WalletAsset; 3] = [
    WalletAsset {
        platform: "macOS, Apple silicon",
        file: "rill-wallet-darwin-arm64",
    },
    WalletAsset {
        platform: "macOS, Intel",
        file: "rill-wallet-darwin-x64",
    },
    WalletAsset {
        platform: "Linux, x86_64",
        file: "rill-wallet-linux-x64",
    },
];

/// Just the filenames, for a caller that checks a list of names against another list of names.
pub fn wallet_asset_files() -> [&'static str; 3] {
    [
        WALLET_ASSETS[0].file,
        WALLET_ASSETS[1].file,
        WALLET_ASSETS[2].file,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every asset is a platform build of the one binary. An asset named anything else would be
    /// downloaded, renamed to `rill-wallet` by the install line, and then be some other program.
    #[test]
    fn every_asset_is_a_build_of_the_named_binary() {
        for asset in WALLET_ASSETS {
            assert!(
                asset.file.starts_with(WALLET_BINARY),
                "{} is not a build of {WALLET_BINARY}",
                asset.file
            );
            assert!(
                !asset.platform.is_empty(),
                "{} has no platform a reader could match against their machine",
                asset.file
            );
        }
    }

    /// The download URL must be under the origin the repository publishes to. Two constants that
    /// name the same place can disagree, and the one a reader acts on is this one.
    #[test]
    fn the_download_url_is_under_the_one_origin() {
        assert!(
            LATEST_DOWNLOAD_URL.starts_with(RELEASES_URL),
            "{LATEST_DOWNLOAD_URL} is not under {RELEASES_URL}"
        );
        assert!(
            RELEASES_URL.contains(RELEASE_REPO),
            "{RELEASES_URL} does not name {RELEASE_REPO}"
        );
    }
}
