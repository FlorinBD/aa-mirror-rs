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