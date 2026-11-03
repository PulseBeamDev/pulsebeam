use super::*;
use agent_core::client_sdp::{Binding, Encoding, Error, RtcpParameters, RuntimeEncoding};
use std::collections::BTreeSet;

fn property(object: &JsValue, name: &str) -> Result<Option<JsValue>, Error> {
    let value = Reflect::get(object, &JsValue::from_str(name)).map_err(|_| Error::Observation)?;
    Ok((!value.is_null() && !value.is_undefined()).then_some(value))
}

fn string(object: &JsValue, name: &str) -> Result<Option<String>, Error> {
    property(object, name)?
        .map(|value| value.as_string().ok_or(Error::Observation))
        .transpose()
}

fn boolean(object: &JsValue, name: &str) -> Result<Option<bool>, Error> {
    property(object, name)?
        .map(|value| value.as_bool().ok_or(Error::Observation))
        .transpose()
}

fn number(object: &JsValue, name: &str) -> Result<Option<f64>, Error> {
    property(object, name)?
        .map(|value| {
            value
                .as_f64()
                .filter(|number| number.is_finite())
                .ok_or(Error::Observation)
        })
        .transpose()
}

fn unsigned(object: &JsValue, name: &str) -> Result<Option<u32>, Error> {
    number(object, name)?
        .map(|value| {
            if value.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&value) {
                Ok(value as u32)
            } else {
                Err(Error::Observation)
            }
        })
        .transpose()
}

pub(super) fn observe_stats(
    report: &JsValue,
    binding: &Binding,
    encodings: &mut [Encoding],
) -> Result<(), Error> {
    let values = property(report, "values")?
        .ok_or(Error::Observation)?
        .dyn_into::<Function>()
        .map_err(|_| Error::Observation)?
        .call0(report)
        .map_err(|_| Error::Observation)?;
    let values = js_sys::try_iter(&values)
        .map_err(|_| Error::Observation)?
        .ok_or(Error::Observation)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Error::Observation)?;
    let mut codecs = BTreeSet::new();
    for value in &values {
        if string(value, "type")?.as_deref() == Some("codec")
            && unsigned(value, "payloadType")? == Some(u32::from(binding.codec.payload_type))
        {
            codecs.insert(string(value, "id")?.ok_or(Error::Observation)?);
        }
    }
    let mut seen = BTreeSet::new();
    for value in &values {
        if string(value, "type")?.as_deref() != Some("outbound-rtp")
            || string(value, "mid")?.as_deref() != Some(binding.mid.as_str())
            || !string(value, "codecId")?.is_some_and(|id| codecs.contains(&id))
        {
            continue;
        }
        let rid = string(value, "rid")?.filter(|rid| !rid.is_empty());
        let Some((index, encoding)) = encodings
            .iter_mut()
            .enumerate()
            .find(|(_, encoding)| encoding.rid == rid)
        else {
            return Err(Error::Observation);
        };
        if !seen.insert(index) {
            return Err(Error::Observation);
        }
        let primary = unsigned(value, "ssrc")?;
        let repair = unsigned(value, "rtxSsrc")?;
        if primary.is_some() && encoding.primary.is_some() && primary != encoding.primary
            || repair.is_some() && encoding.repair.is_some() && repair != encoding.repair
        {
            return Err(Error::Observation);
        }
        encoding.primary = encoding.primary.or(primary);
        encoding.repair = encoding.repair.or(repair);
        if let Some(mode) = string(value, "scalabilityMode")? {
            if encoding
                .runtime
                .scalability_mode
                .as_ref()
                .is_some_and(|known| *known != mode)
            {
                return Err(Error::Observation);
            }
            encoding.runtime.scalability_mode = Some(mode);
        }
    }
    Ok(())
}

pub(super) fn sender_facts(
    sender: &RtcRtpSender,
    binding: &Binding,
) -> Result<(Vec<Encoding>, RtcpParameters), Error> {
    let parameters = sender.get_parameters();
    let values = property(parameters.as_ref(), "encodings")?
        .ok_or(Error::Observation)?
        .dyn_into::<Array>()
        .map_err(|_| Error::Observation)?;
    let encodings = values
        .iter()
        .map(|value| {
            let rid = string(&value, "rid")?.filter(|rid| !rid.is_empty());
            let previous = binding
                .encodings
                .iter()
                .find(|previous| previous.rid == rid)
                .or_else(|| {
                    if binding.encodings.len() == 1 && rid.is_none() {
                        binding.encodings.first()
                    } else {
                        None
                    }
                })
                .ok_or(Error::Observation)?;
            let repair = property(&value, "rtx")?
                .map(|rtx| unsigned(&rtx, "ssrc"))
                .transpose()?
                .flatten();
            Ok(Encoding {
                rid,
                primary: unsigned(&value, "ssrc")?,
                repair,
                paused: previous.paused,
                runtime: RuntimeEncoding {
                    active: boolean(&value, "active")?,
                    max_bitrate: unsigned(&value, "maxBitrate")?.map(u64::from),
                    max_framerate: number(&value, "maxFramerate")?,
                    scalability_mode: string(&value, "scalabilityMode")?,
                },
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let rtcp = if let Some(rtcp) = property(parameters.as_ref(), "rtcp")? {
        RtcpParameters {
            cname: string(&rtcp, "cname")?.filter(|cname| !cname.is_empty()),
            reduced_size: boolean(&rtcp, "reducedSize")?.unwrap_or(binding.rtcp.reduced_size),
        }
    } else {
        RtcpParameters {
            cname: None,
            reduced_size: binding.rtcp.reduced_size,
        }
    };
    Ok((encodings, rtcp))
}
