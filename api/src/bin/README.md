Usage

# Convert any bincode file in place — pass its path; JSON lands next to it as <path>.json:
cargo run --bin convertor -- temp/winds/icon_eu/2026-05-25/06-00-00
# → converted temp/winds/icon_eu/2026-05-25/06-00-00 (5 time slots) → temp/winds/icon_eu/2026-05-25/06-00-00.json

Output looks like:
{
"2026-05-25T06:00:00+00:00": {
"85000": [
{ "lat": 50.4, "lon": 30.5, "u_wind": 3.1, "v_wind": -4.2 }
]
}
}