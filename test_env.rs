// Quick test to verify .env credentials
use std::env;

fn main() {
    // Load .env
    let _ = dotenvy::dotenv();

    let username = env::var("SLSK_USERNAME").expect("SLSK_USERNAME not set");
    let password = env::var("SLSK_PASSWORD").expect("SLSK_PASSWORD not set");

    println!("Username: '{}'", username);
    println!("Username length: {}", username.len());
    println!("Username bytes: {:?}", username.as_bytes());

    println!("\nPassword length: {}", password.len());
    println!("Password first char: {:?}", password.chars().next());
    println!("Password last char: {:?}", password.chars().last());

    // Check for whitespace
    if username.trim() != username {
        println!("⚠️  WARNING: Username has leading/trailing whitespace!");
    }
    if password.trim() != password {
        println!("⚠️  WARNING: Password has leading/trailing whitespace!");
    }

    // Show MD5 hash
    let combined = format!("{}{}", username, password);
    let md5_hash = format!("{:x}", md5::compute(combined.as_bytes()));
    println!("\nMD5(username+password): {}", md5_hash);
}
