use quick_xml::Reader;
use quick_xml::events::Event;

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
    out
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
}

impl Element {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    pub fn with_text(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
            ..Self::default()
        }
    }

    /// Local name without a namespace prefix.
    pub fn local_name(&self) -> &str {
        self.name.rsplit_once(':').map_or(&self.name, |(_, n)| n)
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.local_name() == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.local_name() == name)
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn write(&self, out: &mut String) {
        out.push('<');
        out.push_str(&self.name);
        for (k, v) in &self.attrs {
            out.push(' ');
            out.push_str(k);
            out.push_str("=\"");
            out.push_str(&escape(v));
            out.push('"');
        }
        if self.children.is_empty() && self.text.is_empty() {
            out.push_str("/>");
            return;
        }
        out.push('>');
        out.push_str(&escape(&self.text));
        for c in &self.children {
            c.write(out);
        }
        out.push_str("</");
        out.push_str(&self.name);
        out.push('>');
    }

    pub fn to_xml(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }
}

pub fn parse(input: &[u8]) -> Result<Element, String> {
    let mut reader = Reader::from_reader(input);
    let mut stack: Vec<Element> = vec![Element::new("#document")];
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| e.to_string())?;
        match event {
            Event::Start(e) => stack.push(start_element(&e)?),
            Event::Empty(e) => {
                let el = start_element(&e)?;
                stack.last_mut().expect("document root").children.push(el);
            }
            Event::End(_) => {
                let el = stack.pop().ok_or("unbalanced xml")?;
                stack.last_mut().ok_or("unbalanced xml")?.children.push(el);
            }
            Event::Text(t) => {
                let text = t.decode().map_err(|e| e.to_string())?;
                let text = quick_xml::escape::unescape(&text).map_err(|e| e.to_string())?;
                stack
                    .last_mut()
                    .expect("document root")
                    .text
                    .push_str(&text);
            }
            Event::CData(t) => {
                let text = String::from_utf8_lossy(&t).into_owned();
                stack
                    .last_mut()
                    .expect("document root")
                    .text
                    .push_str(&text);
            }
            Event::GeneralRef(r) => {
                let name = r.decode().map_err(|e| e.to_string())?;
                let resolved =
                    resolve_entity(&name).ok_or_else(|| format!("unknown entity &{name};"))?;
                stack
                    .last_mut()
                    .expect("document root")
                    .text
                    .push_str(&resolved);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let mut doc = stack.pop().ok_or("unbalanced xml")?;
    if !stack.is_empty() {
        return Err("unclosed element".to_string());
    }
    doc.children
        .pop()
        .ok_or_else(|| "empty xml document".to_string())
}

fn resolve_entity(name: &str) -> Option<String> {
    if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        return char::from_u32(u32::from_str_radix(hex, 16).ok()?).map(String::from);
    }
    if let Some(dec) = name.strip_prefix('#') {
        return char::from_u32(dec.parse().ok()?).map(String::from);
    }
    quick_xml::escape::resolve_predefined_entity(name).map(str::to_string)
}

fn start_element(e: &quick_xml::events::BytesStart<'_>) -> Result<Element, String> {
    let mut el = Element::new(String::from_utf8_lossy(e.name().as_ref()).into_owned());
    for a in e.attributes() {
        let a = a.map_err(|e| e.to_string())?;
        let value = a.unescape_value().map_err(|e| e.to_string())?.into_owned();
        el.attrs
            .push((String::from_utf8_lossy(a.key.as_ref()).into_owned(), value));
    }
    Ok(el)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_writes() {
        let el = parse(b"<a x=\"1\"><b>t&amp;u</b><c/></a>").unwrap();
        assert_eq!(el.child("b").unwrap().text, "t&u");
        assert_eq!(el.to_xml(), "<a x=\"1\"><b>t&amp;u</b><c/></a>");
    }
}
