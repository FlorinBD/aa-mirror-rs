use std::borrow::Cow;

pub fn parse_response_lines(rsp: Vec<u8>) -> Result<Vec<String>, String> {
    // Convert bytes to UTF-8 safely, replacing invalid bytes with �
    let s = String::from_utf8_lossy(&rsp);

    // Iterate over lines, take first tab-separated column, skip empty lines
    let response: Vec<String> = s
        .lines()
        .filter_map(|line| line.split('\t').next()) // first column
        .filter(|col| !col.is_empty())             // skip empty
        .map(|col| col.to_string())                // make owned
        .collect();

    Ok(response)
}

pub fn parse_response_lines_old(rsp: Vec<u8>) ->Result<Vec<String>, String>
{
    let lines=String::from_utf8_lossy(&rsp).to_string();
    let mut response = vec![];
    if !lines.is_empty() {
        lines.lines().into_iter().for_each(|line| {
            let parts: Vec<&str> = line.split("\t").collect();
            if !parts.is_empty() {
                response.push(parts[0].to_string());
            }
        })
    };
    Ok(response)
}