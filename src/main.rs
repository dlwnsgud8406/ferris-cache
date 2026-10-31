mod server;
mod command;
mod protocol;

fn main() -> std::io::Result<()>{
    server::run("127.0.0.1:6379")
}
