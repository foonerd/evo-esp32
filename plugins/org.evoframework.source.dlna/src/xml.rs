// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The little of XML that UPnP needs: a device description, a SOAP
//! envelope, a DIDL-Lite listing. Elements, attributes, text and the
//! five entities (and numeric ones), read into a tree; comments,
//! processing instructions, CDATA and doctypes are passed over. Names
//! keep their prefix; [`Node::local`] compares without it, since every
//! server picks its own.

/// An element: its name as written, its attributes, its text (the
/// character data directly inside it, entities decoded) and its
/// children, in order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Node {
    /// The name as written, prefix and all.
    pub name: String,
    /// The attributes, in order, values decoded.
    pub attrs: Vec<(String, String)>,
    /// The character data directly inside, decoded.
    pub text: String,
    /// The child elements, in order.
    pub children: Vec<Node>,
}

impl Node {
    /// The name without its namespace prefix.
    pub fn local(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(&self.name)
    }

    /// An attribute by name (as written, prefix and all).
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The first child whose local name is `local`.
    pub fn child(&self, local: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.local() == local)
    }

    /// The text of the first child called `local`, trimmed, if any.
    pub fn text_of(&self, local: &str) -> Option<&str> {
        self.child(local)
            .map(|c| c.text.trim())
            .filter(|t| !t.is_empty())
    }

    /// Every node in the tree, depth first, whose local name is `local`.
    pub fn find_all<'a>(&'a self, local: &str, out: &mut Vec<&'a Node>) {
        if self.local() == local {
            out.push(self);
        }
        for c in &self.children {
            c.find_all(local, out);
        }
    }

    /// The first node in the tree, depth first, called `local`.
    pub fn find(&self, local: &str) -> Option<&Node> {
        if self.local() == local {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(local))
    }
}

/// Parse a document to its root element.
pub fn parse(text: &str) -> Result<Node, String> {
    let mut stack: Vec<Node> = vec![Node::default()];
    let mut rest = text;
    // Trailing text outside the root is ignored.
    while let Some(lt) = rest.find('<') {
        let (text_before, after) = rest.split_at(lt);
        if !text_before.trim().is_empty() {
            let top = stack.last_mut().ok_or("text outside the root")?;
            top.text.push_str(&unescape(text_before));
        }
        rest = after;
        if let Some(r) = rest.strip_prefix("<!--") {
            let end = r.find("-->").ok_or("an unclosed comment")?;
            rest = &r[end + 3..];
        } else if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>").ok_or("an unclosed CDATA")?;
            let top = stack.last_mut().ok_or("text outside the root")?;
            top.text.push_str(&r[..end]);
            rest = &r[end + 3..];
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            let end = rest.find('>').ok_or("an unclosed declaration")?;
            rest = &rest[end + 1..];
        } else if let Some(r) = rest.strip_prefix("</") {
            let end = r.find('>').ok_or("an unclosed end tag")?;
            let name = r[..end].trim();
            let done = stack.pop().ok_or("an end tag with no start")?;
            if done.name != name {
                return Err(format!("<{}> closed by </{name}>", done.name));
            }
            let parent = stack.last_mut().ok_or("an end tag at the root")?;
            parent.children.push(done);
            rest = &r[end + 1..];
        } else {
            let (node, selfclose, used) = start_tag(&rest[1..])?;
            rest = &rest[1 + used..];
            if selfclose {
                let parent = stack.last_mut().ok_or("a tag at the root")?;
                parent.children.push(node);
            } else {
                stack.push(node);
            }
        }
    }
    if stack.len() != 1 {
        return Err(format!("<{}> never closed", stack.last().unwrap().name));
    }
    let mut doc = stack.pop().unwrap();
    if doc.children.is_empty() {
        return Err("no root element".to_string());
    }
    Ok(doc.children.remove(0))
}

/// A start tag's body after `<`: the node, whether it closed itself,
/// and how many bytes were used up to and including `>`.
fn start_tag(s: &str) -> Result<(Node, bool, usize), String> {
    let mut node = Node::default();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len()
        && !bytes[i].is_ascii_whitespace()
        && bytes[i] != b'>'
        && bytes[i] != b'/'
    {
        i += 1;
    }
    node.name = s[..i].to_string();
    if node.name.is_empty() {
        return Err("a tag with no name".to_string());
    }
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(format!("<{} never closed", node.name));
        }
        match bytes[i] {
            b'>' => return Ok((node, false, i + 1)),
            b'/' => {
                let close = s[i + 1..].find('>').ok_or("an unclosed tag")?;
                return Ok((node, true, i + 1 + close + 1));
            }
            _ => {
                let start = i;
                while i < bytes.len()
                    && bytes[i] != b'='
                    && !bytes[i].is_ascii_whitespace()
                    && bytes[i] != b'>'
                {
                    i += 1;
                }
                let key = s[start..i].to_string();
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= bytes.len() || bytes[i] != b'=' {
                    // An attribute without a value; keep it empty.
                    node.attrs.push((key, String::new()));
                    continue;
                }
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                let quote =
                    *bytes.get(i).ok_or("an attribute without its value")?;
                if quote != b'"' && quote != b'\'' {
                    return Err(format!("attribute {key} is not quoted"));
                }
                let end = s[i + 1..]
                    .find(quote as char)
                    .ok_or("an unclosed attribute value")?;
                node.attrs.push((key, unescape(&s[i + 1..i + 1 + end])));
                i += 1 + end + 1;
            }
        }
    }
}

/// Character data with its entities decoded.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi) = after.find(';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let entity = &after[..semi];
        let decoded = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| {
                    n.strip_prefix('x').map_or_else(
                        || n.parse::<u32>().ok(),
                        |h| u32::from_str_radix(h, 16).ok(),
                    )
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => out.push(c),
            None => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// Text for an element's or attribute's content.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_description_reads() {
        let xml = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><device><deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType><friendlyName>Bench &amp; DLNA</friendlyName><UDN>uuid:4d696e69-444c-4e41-9d41-000000000001</UDN><dlna:X_DLNADOC xmlns:dlna="urn:schemas-dlna-org:device-1-0">DMS-1.50</dlna:X_DLNADOC><iconList><icon><mimetype>image/png</mimetype><url>/icons/sm.png</url></icon></iconList><serviceList><service><serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType><controlURL>/ctl/ContentDir</controlURL></service><service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType><controlURL>/ctl/ConnectionMgr</controlURL></service></serviceList></device></root>"#;
        let root = parse(xml).unwrap();
        assert_eq!(root.name, "root");
        let device = root.child("device").unwrap();
        assert_eq!(device.text_of("friendlyName"), Some("Bench & DLNA"));
        assert_eq!(
            device.text_of("UDN"),
            Some("uuid:4d696e69-444c-4e41-9d41-000000000001")
        );
        let mut services = Vec::new();
        root.find_all("service", &mut services);
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].text_of("controlURL"), Some("/ctl/ContentDir"));
        assert_eq!(
            root.find("X_DLNADOC").map(|n| n.name.as_str()),
            Some("dlna:X_DLNADOC")
        );
    }

    #[test]
    fn didl_lite_reads_with_attributes_entities_and_self_closing_tags() {
        let didl = "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\">\n<container id=\"64\" parentID=\"0\" restricted=\"1\" childCount=\"5\"><dc:title>Browse Folders</dc:title><upnp:class>object.container.storageFolder</upnp:class></container><item id=\"64$0\" parentID=\"64\" restricted=\"1\"><dc:title>Rock &amp; Roll &#x2013; Live</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><upnp:artist>Bench Signals</upnp:artist><upnp:album>Envelopes</upnp:album><upnp:albumArtURI dlna:profileID=\"JPEG_TN\" xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\">http://h/AlbumArt/1.jpg</upnp:albumArtURI><res size=\"532957\" duration=\"0:00:45.000\" protocolInfo=\"http-get:*:audio/x-flac:*\">http://192.0.2.10:8200/MediaItems/22.flac</res><desc/></item></DIDL-Lite>";
        let root = parse(didl).unwrap();
        assert_eq!(root.local(), "DIDL-Lite");
        let container = root.child("container").unwrap();
        assert_eq!(container.attr("id"), Some("64"));
        assert_eq!(container.text_of("title"), Some("Browse Folders"));
        let item = root.child("item").unwrap();
        assert_eq!(item.text_of("title"), Some("Rock & Roll \u{2013} Live"));
        let res = item.child("res").unwrap();
        assert_eq!(res.attr("duration"), Some("0:00:45.000"));
        assert_eq!(
            res.text.trim(),
            "http://192.0.2.10:8200/MediaItems/22.flac"
        );
        assert_eq!(
            item.text_of("albumArtURI"),
            Some("http://h/AlbumArt/1.jpg")
        );
        assert!(item.child("desc").is_some());
    }

    #[test]
    fn a_soap_result_unescapes_to_a_document() {
        let soap = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\"><Result>&lt;DIDL-Lite&gt;&lt;item id=\"1\"&gt;&lt;dc:title&gt;A&lt;/dc:title&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</Result><NumberReturned>1</NumberReturned><TotalMatches>7</TotalMatches></u:BrowseResponse></s:Body></s:Envelope>";
        let env = parse(soap).unwrap();
        let result = env.find("Result").unwrap();
        let didl = parse(&result.text).unwrap();
        assert_eq!(didl.child("item").unwrap().text_of("title"), Some("A"));
        assert_eq!(env.find("TotalMatches").unwrap().text, "7");
        assert!(parse("<a><b></a>").is_err());
        assert!(parse("no xml").is_err());
    }
}
