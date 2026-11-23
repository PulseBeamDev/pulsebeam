use super::{
    Sender, media_extension_id, repair_header_extensions, retains_repair_extension,
    sender_extension_form,
};
use crate::{RtpExtensionForm, RtpWrite};

#[derive(Clone, Copy)]
pub(super) struct PacketLayout {
    pub(super) bytes: usize,
    pub(super) extensions: Option<ExtensionLayout>,
}

#[derive(Clone, Copy)]
pub(super) struct ExtensionLayout {
    pub(super) one_byte: bool,
    pub(super) words: u16,
}

impl PacketLayout {
    pub(super) fn original(sender: &Sender, media: &RtpWrite, emit_mid: bool) -> Option<Self> {
        let forwarded = media.extensions.iter().filter_map(|value| {
            media_extension_id(sender, &value.uri).map(|id| (id, value.value.len()))
        });
        let mid = sender
            .facts
            .mid_extension_id
            .filter(|_| emit_mid)
            .map(|id| (id, sender.facts.mid.len()));
        let rid_id = if sender.ssrc == sender.rtx_ssrc {
            sender.repaired_rid_extension_id
        } else {
            sender.rid_extension_id
        };
        let rid = rid_id
            .zip(sender.rid.as_ref())
            .map(|(id, rid)| (id, rid.len()));
        let twcc = sender.facts.twcc_extension_id.map(|id| (id, 2));
        Self::new(
            media.payload.len(),
            forwarded.chain(mid).chain(rid).chain(twcc),
            sender_extension_form(sender),
        )
    }

    pub(super) fn repair(sender: &Sender, media: &RtpWrite, emit_mid: bool) -> Option<Self> {
        let forwarded = media.extensions.iter().filter_map(|value| {
            let id = media_extension_id(sender, &value.uri)?;
            retains_repair_extension(sender, id).then_some((id, value.value.len()))
        });
        Self::new(
            media.payload.len().checked_add(2)?,
            forwarded.chain(repair_header_extensions(sender, emit_mid)),
            sender_extension_form(sender),
        )
    }

    pub(super) fn new(
        payload: usize,
        mut extensions: impl Iterator<Item = (u8, usize)> + Clone,
        form: RtpExtensionForm,
    ) -> Option<Self> {
        let base = 12usize.checked_add(payload)?;
        if extensions.clone().next().is_none() {
            return Some(Self {
                bytes: base,
                extensions: None,
            });
        }
        if extensions
            .clone()
            .any(|(id, len)| id == 0 || len > usize::from(u8::MAX))
        {
            return None;
        }
        let fits_one_byte = || {
            extensions
                .clone()
                .all(|(id, len)| (1..=14).contains(&id) && (1..=16).contains(&len))
        };
        let one_byte = match form {
            RtpExtensionForm::OneByte if fits_one_byte() => true,
            RtpExtensionForm::OneByte => return None,
            RtpExtensionForm::TwoByte => false,
            RtpExtensionForm::Mixed => fits_one_byte(),
        };
        let data = extensions.try_fold(0usize, |bytes, (_, len)| {
            bytes
                .checked_add(if one_byte { 1 } else { 2 })?
                .checked_add(len)
        })?;
        let words = u16::try_from(data.checked_add(3)?.checked_div(4)?).ok()?;
        let bytes = base
            .checked_add(4)?
            .checked_add(usize::from(words).checked_mul(4)?)?;
        Some(Self {
            bytes,
            extensions: Some(ExtensionLayout { one_byte, words }),
        })
    }
}
