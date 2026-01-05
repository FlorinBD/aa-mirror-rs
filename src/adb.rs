pub fn parse_response_lines(rsp: Vec<u8>) ->Result<Vec<String>, String>
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