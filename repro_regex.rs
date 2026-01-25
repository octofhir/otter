use regex::Regex;

fn main() {
    let pattern = "[--\\d]+";
    // Simulate the patch logic
    let rust_pattern = pattern.replace("[--", "[\\-\\-");
    println!("Original: '{}'", pattern);
    println!("Patched:  '{}'", rust_pattern);

    let re = Regex::new(&rust_pattern).unwrap();
    let input = ".-0123456789-.";

    if let Some(captures) = re.captures(input) {
        println!("Match found: {:?}", captures.get(0).map(|m| m.as_str()));
    } else {
        println!("No match found");
    }
}
