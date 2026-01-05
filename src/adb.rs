use std::borrow::Cow;

pub fn parse_response_lines(rsp: Vec<u8>) -> Result<Vec<Cow<'static, str>>, String> {
    // Convert bytes to UTF-8 lossily
    let s: Cow<str> = String::from_utf8_lossy(&rsp);

    // Collect first tab-separated column of each line as Cow<str>
    let response: Vec<Cow<'static, str>> = s
        .lines()
        .filter_map(|line| line.split('\t').next()) // take first column
        .map(|col| Cow::Owned(col.to_string()))     // convert to owned Cow<str>
        .collect();

    Ok(response)
}