pub(crate) fn p256_fixed_to_der(signature: &[u8]) -> Result<Vec<u8>, &'static str> {
    if signature.len() != 64 {
        return Err("P-256 signature must contain 64 bytes");
    }
    let r = der_integer(&signature[..32]);
    let s = der_integer(&signature[32..]);
    let payload_len = r.len() + s.len();
    if payload_len > 127 {
        return Err("P-256 DER signature is unexpectedly large");
    }
    let mut der = Vec::with_capacity(payload_len + 2);
    der.extend_from_slice(&[
        0x30,
        u8::try_from(payload_len).expect("P-256 DER payload fits one byte"),
    ]);
    der.extend_from_slice(&r);
    der.extend_from_slice(&s);
    Ok(der)
}

fn der_integer(value: &[u8]) -> Vec<u8> {
    let first_nonzero = value
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(value.len() - 1);
    let value = &value[first_nonzero..];
    let leading_zero = value[0] & 0x80 != 0;
    let mut der = Vec::with_capacity(value.len() + usize::from(leading_zero) + 2);
    der.extend_from_slice(&[
        0x02,
        u8::try_from(value.len() + usize::from(leading_zero)).expect("P-256 integer fits one byte"),
    ]);
    if leading_zero {
        der.push(0);
    }
    der.extend_from_slice(value);
    der
}

#[cfg(test)]
mod tests {
    use super::p256_fixed_to_der;

    #[test]
    fn fixed_width_p256_signature_becomes_canonical_der() {
        let mut signature = [0_u8; 64];
        signature[31] = 1;
        signature[32] = 0x80;
        let der = p256_fixed_to_der(&signature).unwrap();
        let mut expected = vec![0x30, 0x26, 0x02, 0x01, 0x01, 0x02, 0x21, 0x00, 0x80];
        expected.extend_from_slice(&[0; 31]);
        assert_eq!(der, expected);
    }

    #[test]
    fn rejects_non_p256_fixed_width_signature() {
        assert!(p256_fixed_to_der(&[0; 63]).is_err());
    }
}
