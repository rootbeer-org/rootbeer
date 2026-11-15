use super::git::Tag;
use std::collections::BTreeSet;

const LIMIT: u64 = 16 << 20;

/// Every name a download page links to, as a tag without a commit. A name is
/// the last part of a link's path, so a file and a directory both count.
pub(super) fn tags(agent: &ureq::Agent, url: &str) -> Result<Vec<Tag>, String> {
    let page = agent
        .get(url)
        .call()
        .map_err(|error| format!("{url}: {error}"))?
        .into_body()
        .into_with_config()
        .limit(LIMIT)
        .read_to_string()
        .map_err(|error| format!("{url}: {error}"))?;

    let tags = names(&page)
        .into_iter()
        .map(|name| Tag {
            name,
            commit: String::new(),
        })
        .collect();
    Ok(tags)
}

/// The last path part of each `href` in a page, without a query or fragment.
fn names(page: &str) -> BTreeSet<String> {
    let lower = page.to_ascii_lowercase();
    let mut names = BTreeSet::new();
    let mut rest = 0;
    while let Some(found) = lower[rest..].find("href") {
        rest += found + "href".len();
        let Some(value) = page[rest..].trim_start().strip_prefix('=') else {
            continue;
        };

        let value = value.trim_start();
        let Some(quote) = value
            .chars()
            .next()
            .filter(|quote| matches!(quote, '"' | '\''))
        else {
            continue;
        };

        let Some(link) = value[1..].split(quote).next() else {
            continue;
        };

        let path = link.split(['?', '#']).next().unwrap_or_default();
        let name = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        if !name.is_empty() {
            names.insert(name.to_string());
        }
    }

    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_the_last_path_part_of_each_link() {
        let page = r##"<html><body>
            <a href="nmap-7.98.tgz">nmap-7.98.tgz</a>
            <A HREF = 'https://nmap.org/dist/nmap-7.99.tgz?download=1#top'>newer</A>
            <a href="nmap-7.99.tgz">again</a>
            <a href="v1.2.3/">a directory</a>
            <a href="/">root</a>
            <a href=unquoted.tgz>unquoted</a>
            <a name="href">no link</a>
        </body></html>"##;

        let names: Vec<String> = names(page).into_iter().collect();
        assert_eq!(names, ["nmap-7.98.tgz", "nmap-7.99.tgz", "v1.2.3"]);
    }
}
