use super::*;

pub(super) fn handshake(socket: &Path) -> io::Result<Handshake> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(DAEMON_HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(DAEMON_HANDSHAKE_TIMEOUT))?;
    stream.write_all(
        format!(
            "{{\"type\":\"hello\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}\n"
        )
        .as_bytes(),
    )?;
    stream.flush()?;

    let response = read_line(&mut stream)?;
    if !response.contains("\"type\":\"hello_ack\"")
        || !response.contains(&format!("\"protocol\":\"{PROTOCOL}\""))
        || !response.contains(&format!("\"version\":{PROTOCOL_VERSION}"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon handshake response did not match the expected protocol",
        ));
    }

    Ok(Handshake {
        daemon_version: extract_json_string(&response, "daemonVersion")
            .unwrap_or_else(|| "unknown".to_string()),
        sync_json: extract_json_object(&response, "sync"),
    })
}

pub(super) fn request_shutdown(socket: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        format!(
            "{{\"type\":\"shutdown\",\"protocol\":\"{PROTOCOL}\",\"version\":{PROTOCOL_VERSION}}}\n"
        )
        .as_bytes(),
    )?;
    stream.flush()?;

    let response = read_line(&mut stream)?;
    if !response.contains("\"type\":\"shutdown_ack\"")
        || !response.contains(&format!("\"protocol\":\"{PROTOCOL}\""))
        || !response.contains(&format!("\"version\":{PROTOCOL_VERSION}"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon shutdown response did not match the expected protocol",
        ));
    }
    Ok(())
}

pub(super) fn read_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut one = [0_u8; 1];
    loop {
        match stream.read(&mut one) {
            Ok(0) => break,
            Ok(_) if one[0] == b'\n' => break,
            Ok(_) => bytes.push(one[0]),
            Err(error) => return Err(error),
        }
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(super) fn extract_json_string(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = input.find(&needle)? + needle.len();
    let mut value = String::new();
    let mut escaped = false;

    for character in input[start..].chars() {
        if escaped {
            value.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => return Some(value),
            _ => value.push(character),
        }
    }

    None
}

pub(super) fn extract_json_object(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let marker_start = input.find(&needle)?;
    let object_start =
        marker_start + needle.len() + input[marker_start + needle.len()..].find('{')?;
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, character) in input[object_start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = object_start + offset + character.len_utf8();
                    return Some(input[object_start..end].to_string());
                }
            }
            _ => {}
        }
    }

    None
}
