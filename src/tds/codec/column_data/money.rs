use crate::{error::Error, numeric::Numeric, sql_read_bytes::SqlReadBytes, ColumnData};

/// Decode 'money' and 'smallmoney' types according to the TDS spec.
///
/// See: <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-tds/1266679d-cd6e-492a-b2b2-3a9ba004196d>.
pub(crate) async fn decode<R>(src: &mut R, len: u8) -> crate::Result<ColumnData<'static>>
where
    R: SqlReadBytes + Unpin,
{
    /// According to the TDS spec both money types are sent over the wire with
    /// a scale of 4 (aka ten-thousandth).
    const SCALE: u8 = 4;

    let res = match len {
        0 => ColumnData::Numeric(None),
        4 => {
            let value = src.read_i32_le().await?;
            let value = Numeric::new_with_scale(value.into(), SCALE);
            ColumnData::Numeric(Some(value))
        }
        8 => {
            let high = src.read_i32_le().await?;
            let low = src.read_u32_le().await?;

            let value = (high as i64) << 32 | (low as i64);
            let value = Numeric::new_with_scale(value.into(), SCALE);
            ColumnData::Numeric(Some(value))
        }
        _ => {
            return Err(Error::Protocol(
                format!("money: length of {} is invalid", len).into(),
            ))
        }
    };

    Ok(res)
}
