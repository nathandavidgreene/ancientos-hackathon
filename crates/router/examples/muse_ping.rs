//! Ping the Meta Model API with a minimal Muse Spark chat call.
//!
//! Requires `META_API_KEY` and `ANCIENTOS_ALLOW_PAID=1` in the environment.
//! Prints only the model id, the reply text, and token usage.

use ancientos_router::data_class::DataClass;
use ancientos_router::muse::MuseClient;

/// Example: send one ping to Muse (paid, gated).
fn main() {
    let client = MuseClient::from_env().with_max_completion_tokens(256);
    // The ping prompt is a fixed public string, so Public is honest here.
    match client.chat_with_class(DataClass::Public, "muse_ping", None, "Reply with: ok") {
        Ok(resp) => {
            println!("model: {}", resp.model);
            println!("reply: {}", resp.content);
            match &resp.usage {
                Some(usage) => println!("usage: {usage}"),
                None => println!("usage: -"),
            }
        }
        Err(e) => {
            eprintln!("muse_ping failed: {e}");
            std::process::exit(1);
        }
    }
}
