//! RFC 8010 section 3 binary framing. Limits apply before allocating attribute values.
use crate::backend::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub values: Vec<(u8, Vec<u8>)>,
}
impl Attribute {
    pub fn text(name: &str, tag: u8, value: &str) -> Self {
        Self {
            name: name.into(),
            values: vec![(tag, value.as_bytes().to_vec())],
        }
    }
    pub fn integer(name: &str, tag: u8, value: i32) -> Self {
        Self {
            name: name.into(),
            values: vec![(tag, value.to_be_bytes().to_vec())],
        }
    }
    pub fn boolean(name: &str, value: bool) -> Self {
        Self {
            name: name.into(),
            values: vec![(0x22, vec![u8::from(value)])],
        }
    }
    pub fn texts(&self, tag: u8) -> Result<Vec<&str>, Error> {
        self.values
            .iter()
            .map(|(t, v)| {
                if (*t == 0x36 && tag == 0x42) || (*t == 0x35 && tag == 0x41) {
                    let mut cursor = Cursor { bytes: v, at: 0 };
                    cursor.field()?;
                    let text = cursor.field()?;
                    if cursor.at != v.len() {
                        return Err(invalid());
                    }
                    return std::str::from_utf8(text).map_err(|_| invalid());
                }
                if *t != tag {
                    return Err(invalid());
                }
                std::str::from_utf8(v).map_err(|_| invalid())
            })
            .collect()
    }
    /// Media lists may mix keyword and name syntaxes (RFC 8011 section 5.2.11).
    pub fn keyword_or_name_values(&self) -> Result<Vec<(u8, &str)>, Error> {
        self.values
            .iter()
            .map(|(tag, bytes)| match tag {
                0x42 | 0x44 => Ok((*tag, std::str::from_utf8(bytes).map_err(|_| invalid())?)),
                0x36 => {
                    let mut cursor = Cursor { bytes, at: 0 };
                    cursor.field()?;
                    let text = cursor.field()?;
                    if cursor.at != bytes.len() {
                        return Err(invalid());
                    }
                    Ok((0x42, std::str::from_utf8(text).map_err(|_| invalid())?))
                }
                _ => Err(invalid()),
            })
            .collect()
    }
    pub fn number(&self, tag: u8) -> Result<i32, Error> {
        match self.values.as_slice() {
            [(t, v)] if *t == tag => number(v),
            _ => Err(invalid()),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub tag: u8,
    pub attributes: Vec<Attribute>,
}
impl Group {
    pub fn get(&self, name: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.name == name)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub code: u16,
    pub request_id: i32,
    pub groups: Vec<Group>,
}
fn invalid() -> Error {
    Error::Refused(
        "Malformed IPP message. Read fresh status and check the printer's IPP support.".into(),
    )
}
fn number(bytes: &[u8]) -> Result<i32, Error> {
    Ok(i32::from_be_bytes(bytes.try_into().map_err(|_| invalid())?))
}
fn value(tag: u8, bytes: &[u8]) -> Result<(), Error> {
    let valid = match tag {
        0x10 | 0x12 | 0x13 => bytes.is_empty(),
        0x21 | 0x23 => bytes.len() == 4,
        0x22 => matches!(bytes, [0] | [1]),
        0x31 => bytes.len() == 11,
        0x32 => bytes.len() == 9,
        0x33 => {
            bytes.len() == 8
                && number(bytes.get(..4).ok_or_else(invalid)?)?
                    <= number(bytes.get(4..).ok_or_else(invalid)?)?
        }
        0x35 | 0x36 => {
            let mut cursor = Cursor { bytes, at: 0 };
            let language = cursor.field()?;
            let text = cursor.field()?;
            !language.is_empty()
                && language.is_ascii()
                && std::str::from_utf8(text).is_ok()
                && cursor.at == bytes.len()
        }
        0x41 | 0x42 => std::str::from_utf8(bytes).is_ok(),
        0x44..=0x49 => bytes.is_ascii(),
        // Collections are not requested by this client. Refuse nested structures rather than
        // interpreting member values as independent top-level attributes.
        0x34 | 0x37 | 0x4a => false,
        _ => tag >= 0x10 && (tag != 0x7f || bytes.len() >= 4),
    };
    if valid {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn length(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    let n = i16::try_from(bytes.len()).map_err(|_| invalid())?;
    out.extend(n.to_be_bytes());
    out.extend(bytes);
    Ok(())
}
impl Message {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if self.request_id <= 0
            || self.groups.first().is_none_or(|g| g.tag != 1)
            || self.groups.len() > 5002
        {
            return Err(invalid());
        }
        let mut out = vec![1, 1];
        out.extend(self.code.to_be_bytes());
        out.extend(self.request_id.to_be_bytes());
        let mut count = 0usize;
        for group in &self.groups {
            if !matches!(group.tag, 1 | 2 | 4 | 5) || group.attributes.len() > 256 {
                return Err(invalid());
            }
            out.push(group.tag);
            let mut names = std::collections::HashSet::new();
            for a in &group.attributes {
                if a.name.is_empty()
                    || a.name.len() > 255
                    || !a
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    || !names.insert(&a.name)
                    || a.values.is_empty()
                {
                    return Err(invalid());
                }
                for (i, (tag, bytes)) in a.values.iter().enumerate() {
                    count += 1;
                    if count > 20000 {
                        return Err(invalid());
                    }
                    value(*tag, bytes)?;
                    out.push(*tag);
                    length(&mut out, if i == 0 { a.name.as_bytes() } else { b"" })?;
                    length(&mut out, bytes)?;
                    if out.len() > 4 << 20 {
                        return Err(invalid());
                    }
                }
            }
        }
        out.push(3);
        Ok(out)
    }
    /// Returns the offset of document data; responses must have no document data.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), Error> {
        let mut c = Cursor { bytes, at: 0 };
        if !matches!(c.take(2)?, [1, 0] | [1, 1] | [2, 0] | [2, 1] | [2, 2]) {
            return Err(invalid());
        }
        let code = u16::from_be_bytes(c.take(2)?.try_into().map_err(|_| invalid())?);
        let request_id = number(c.take(4)?)?;
        if request_id <= 0 {
            return Err(invalid());
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut count = 0;
        loop {
            if c.at > 4 << 20 {
                return Err(invalid());
            }
            let tag = c.byte()?;
            if tag == 3 {
                if groups.first().is_none_or(|g| g.tag != 1) {
                    return Err(invalid());
                }
                return Ok((
                    Self {
                        code,
                        request_id,
                        groups,
                    },
                    c.at,
                ));
            }
            if tag < 0x10 {
                if !matches!(tag, 1 | 2 | 4 | 5)
                    || groups.len() >= 5002
                    || (tag == 1 && !groups.is_empty())
                {
                    return Err(invalid());
                }
                groups.push(Group {
                    tag,
                    attributes: Vec::new(),
                });
                continue;
            }
            count += 1;
            if count > 20000 {
                return Err(invalid());
            }
            let name = c.field()?;
            let v = c.field()?;
            value(tag, v)?;
            let g = groups.last_mut().ok_or_else(invalid)?;
            if name.is_empty() {
                let a = g.attributes.last_mut().ok_or_else(invalid)?;
                a.values.push((tag, v.to_vec()));
            } else {
                if name.len() > 255
                    || !name
                        .iter()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
                {
                    return Err(invalid());
                }
                let name = std::str::from_utf8(name).map_err(|_| invalid())?.to_owned();
                if g.attributes.len() >= 256 || g.get(&name).is_some() {
                    return Err(invalid());
                }
                g.attributes.push(Attribute {
                    name,
                    values: vec![(tag, v.to_vec())],
                });
            }
        }
    }
}
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.at.checked_add(n).ok_or_else(invalid)?;
        let bytes = self.bytes.get(self.at..end).ok_or_else(invalid)?;
        self.at = end;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8, Error> {
        self.take(1)?.first().copied().ok_or_else(invalid)
    }
    fn field(&mut self) -> Result<&'a [u8], Error> {
        let n = i16::from_be_bytes(self.take(2)?.try_into().map_err(|_| invalid())?);
        self.take(usize::try_from(n).map_err(|_| invalid())?)
    }
}
