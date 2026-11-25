// Copyright © 2025 Tobias J. Prisching <tobias.prisching@icloud.com> and CONTRIBUTORS
// See https://github.com/TechnikTobi/little_exif#license for licensing details

use quick_xml::events::BytesStart;
use quick_xml::events::Event;
use quick_xml::Reader;
use quick_xml::Writer;
use std::io::Cursor;

/// Some images also contain XMP metadata, which in turn may include EXIF data
/// that is simply a duplicate from e.g. the eXIf chunk in a PNG.
/// This function takes in the raw XMP information and removes EXIF attributes,
/// while maintaining other XMP information so that the result can be 
/// written back to the image data.
pub(crate) fn
remove_exif_from_xmp
(
    data: &[u8]
)
-> Result<Vec<u8>, Box<dyn std::error::Error>>
{
    let mut reader = Reader::from_reader(data);
    let mut writer = Writer::new(Cursor::new(Vec::new()));

    // Needed by the reader
    let mut read_buffer = Vec::new();

    // Needed for skipping stuff like 
    // <exif:Description>Hi</exif:Description>\n
    let mut skip_depth   = 0u32;
    let mut skip_next_nl = false;
    let mut element_depth = 0usize;

    loop 
    {
        // Read in the event
        let read_event = reader.read_event_into(&mut read_buffer);

        match read_event
        {
            Ok(Event::Start(ref event)) => {
                element_depth += 1;
                if skip_depth > 0 || event.name().0.starts_with("exif:")
                {
                    skip_depth += 1;
                }
                else if skip_depth == 0
                {
                    writer.write_event(Event::Start(get_exif_filtered_event(event)?))?;
                }
            }

            Ok(Event::Empty(ref event)) => {
                if skip_depth == 0 && !event.name().0.starts_with("exif:")
                {
                    writer.write_event(Event::Empty(get_exif_filtered_event(event)?))?;
                }
            }

            Ok(Event::End(ref event)) => {
                element_depth = element_depth.checked_sub(1).ok_or("unexpected closing XMP element")?;
                if skip_depth > 0 
                {
                    skip_depth  -= 1;
                    skip_next_nl = true;
                } 
                else 
                {
                    writer.write_event(Event::End(event.clone()))?;
                }
            }

            Ok(Event::Eof) => {
                if element_depth != 0 {
                    return Err("unclosed element in XMP".into());
                }
                break;
            }

            Ok(Event::Text(ref event)) => {
                let event_string = event.as_ref();

                let characters = event_string.chars()
                    .filter(|c| *c == '\n' || !c.is_whitespace())
                    .collect::<Vec<char>>();

                if characters == vec!['\n'] && skip_next_nl
                {
                    skip_next_nl = false;
                }
                else if skip_depth == 0 
                {
                    writer.write_event(Event::Text(event.clone()))?; 
                }
            }

            Ok(other_event) => {
                if skip_depth == 0 { writer.write_event(other_event)?; }
            }

            Err(error) => return Err(error.into()),
        };

        read_buffer.clear();
    }

    return Ok(writer.into_inner().into_inner());
}

fn
get_exif_filtered_event<'a>
(
    event: &'a BytesStart<'a>
)
-> Result<BytesStart<'a>, Box<dyn std::error::Error>>
{
    let mut new_event = BytesStart::new(event.name().0);
    for attribute in event.attributes() {
        let attribute = attribute?;
        if !attribute.key.0.starts_with("exif:") {
            new_event.push_attribute(attribute);
        }
    }

    return Ok(new_event);
}

#[cfg(test)]
mod tests {
    use super::remove_exif_from_xmp;

    #[test]
    fn removing_exif_keeps_the_rest_of_the_xmp_packet() {
        let input = br#"<root exif:Orientation="1" label="photo"><exif:Description><child>discard</child></exif:Description><title>keep</title></root>"#;
        let expected = br#"<root label="photo"><title>keep</title></root>"#;
        assert_eq!(remove_exif_from_xmp(input).unwrap(), expected);
    }

    #[test]
    fn malformed_xmp_is_rejected_instead_of_returning_a_partial_packet() {
        assert!(remove_exif_from_xmp(b"<root><exif:Description>unfinished").is_err());
        assert!(remove_exif_from_xmp(b"<root><title>unfinished").is_err());
        assert!(remove_exif_from_xmp(b"<root><title></root>").is_err());
    }
}
