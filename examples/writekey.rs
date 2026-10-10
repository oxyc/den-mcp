//! A fresh token key for library writes (den-spec `wire/assistant-v1.md` §3):
//!
//!   cargo run --example writekey
//!
//! prints `MCP_WRITE_KEY=<secret>` for den-mcp and the matching `MCP_WRITE_PUBLIC_KEY=<public>` for den-edge. The
//! secret is the base64url of 32 random bytes (X-Wing's seed); the public key is its 1,216-byte X-Wing public key.
//! Keep the secret out of shell history and logs: it opens the grant key in every write session's token.

fn main() {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("the platform's CSPRNG");
    println!("MCP_WRITE_KEY={}", den_assistant::b64url(&secret));
    println!("MCP_WRITE_PUBLIC_KEY={}", den_assistant::b64url(&den_assistant::kem_public(&secret)));
}
