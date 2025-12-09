pub mod handshake;


pub trait ClientHandler {
    fn handle(packet: &[u8]);
}