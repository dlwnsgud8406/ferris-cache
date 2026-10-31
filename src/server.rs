use std::io::{self, BufReader, BufWriter, Write};
use std::net::TcpListener;
use std::thread;

use crate::command;
use crate::protocol;


pub fn run(address: &str) -> io::Result<()> {
    let listener = TcpListener::bind(address)?;

    println!("Ferris listening on {}", address);


    Ok(())
}