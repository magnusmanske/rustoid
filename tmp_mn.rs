fn month_number(name: &str) -> Option<usize> {
    const NAMES: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let lower = name.to_lowercase();
    NAMES
        .iter()
        .position(|n| {
            *n == lower || n.starts_with(&lower[..lower.len().min(3)]) && lower.len() >= 3
        })
        .map(|i| i + 1)
}
fn main() {
    for n in ["January", "Jan", "january", "May", "December", "2020"] {
        println!("{} -> {:?}", n, month_number(n));
    }
}
