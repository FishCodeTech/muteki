//! Attachment presentation with core-owned validation semantics and localized errors.

use tcode_core::attachments::AttachError;

/// How this client reaches the machine that stores the attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferLink {
    /// The host runs in this process; nothing crosses a network.
    Local,
    Lan,
    /// A direct path punched across the internet.
    Tunnel,
    /// Carried by a Traverse relay, which is shared and rate-limited.
    Relay,
    /// Remote, but the transport cannot say how (the browser client).
    Unknown,
}

/// What the composer does with an attachment of `size` bytes over `link`,
/// given the device's own ceiling for internet transfers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferVerdict {
    Send,
    /// Over a punched path the user decides; the cost is only their time.
    Confirm,
    /// A relay never carries it: the path is shared with everyone else on
    /// the service and the main stream would stall behind it.
    Reject,
}

pub fn transfer_verdict(link: TransferLink, size: u64, remote_limit: u64) -> TransferVerdict {
    match link {
        TransferLink::Local | TransferLink::Lan => TransferVerdict::Send,
        _ if size <= remote_limit => TransferVerdict::Send,
        TransferLink::Tunnel => TransferVerdict::Confirm,
        TransferLink::Relay | TransferLink::Unknown => TransferVerdict::Reject,
    }
}

pub(crate) fn attach_error_message(error: &AttachError) -> String {
    match error {
        AttachError::UnsupportedType { name } => {
            crate::tr!("attach.unsupported_type", name = name).into_owned()
        }
        AttachError::TooLarge { name } => crate::tr!("attach.too_large", name = name).into_owned(),
        AttachError::TooMany => crate::tr!("attach.too_many").into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_internet_links_apply_the_remote_ceiling() {
        let limit = 2 * 1024 * 1024;
        for link in [TransferLink::Local, TransferLink::Lan] {
            assert_eq!(
                transfer_verdict(link, limit * 5, limit),
                TransferVerdict::Send,
                "{link:?}"
            );
        }
        for link in [
            TransferLink::Tunnel,
            TransferLink::Relay,
            TransferLink::Unknown,
        ] {
            assert_eq!(
                transfer_verdict(link, limit, limit),
                TransferVerdict::Send,
                "{link:?}"
            );
        }
        assert_eq!(
            transfer_verdict(TransferLink::Tunnel, limit + 1, limit),
            TransferVerdict::Confirm
        );
        for link in [TransferLink::Relay, TransferLink::Unknown] {
            assert_eq!(
                transfer_verdict(link, limit + 1, limit),
                TransferVerdict::Reject,
                "{link:?}"
            );
        }
    }
}
