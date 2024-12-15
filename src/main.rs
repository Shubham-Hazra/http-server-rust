use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;

extern crate flate2;
use flate2::write::{GzEncoder, ZlibEncoder};
use flate2::Compression;

#[derive(Debug, PartialEq, Clone, Copy)]
enum CompressionScheme {
    Gzip,
    Zlib,
}

struct CompressionUtil;

impl CompressionUtil {
    fn supported_schemes() -> Vec<CompressionScheme> {
        vec![CompressionScheme::Gzip, CompressionScheme::Zlib]
    }

    fn compress(data: &[u8], scheme: CompressionScheme) -> io::Result<Vec<u8>> {
        match scheme {
            CompressionScheme::Gzip => {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(data)?;
                encoder.finish()
            }
            CompressionScheme::Zlib => {
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(data)?;
                encoder.finish()
            }
        }
    }

    fn scheme_to_header(scheme: CompressionScheme) -> &'static str {
        match scheme {
            CompressionScheme::Gzip => "gzip",
            CompressionScheme::Zlib => "zlib",
        }
    }

    fn negotiate_compression(accept_encoding: &str) -> Option<CompressionScheme> {
        let encodings: Vec<String> = accept_encoding
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .collect();

        Self::supported_schemes().into_iter().find(|&scheme| {
            let scheme_str = Self::scheme_to_header(scheme).to_lowercase();
            encodings.contains(&scheme_str)
        })
    }
}

#[derive(Clone)]
struct ServerConfig {
    port: u16,
    directory: PathBuf,
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

struct HttpResponse {
    status_code: u16,
    status_message: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn new(status_code: u16, status_message: &str) -> Self {
        HttpResponse {
            status_code,
            status_message: status_message.to_string(),
            headers: vec![],
            body: vec![],
        }
    }

    fn with_body(mut self, body: Vec<u8>, content_type: &str) -> Self {
        self.body = body;
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self.headers
            .push(("Content-Length".to_string(), self.body.len().to_string()));
        self
    }

    fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.push((key.to_string(), value.to_string()));
        self
    }

    fn to_bytes(&self) -> Vec<u8> {
        let status_line = format!("HTTP/1.1 {} {}\r\n", self.status_code, self.status_message);
        let headers = self
            .headers
            .iter()
            .map(|(k, v)| format!("{}: {}\r\n", k, v))
            .collect::<String>();

        [
            status_line.as_bytes(),
            headers.as_bytes(),
            b"\r\n",
            &self.body,
        ]
        .concat()
    }
}

struct Router;

impl Router {
    fn parse_request(raw_request: &str) -> io::Result<HttpRequest> {
        let mut lines = raw_request.lines();
        let request_line = lines.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid request: No request line",
            )
        })?;

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid request line",
            ));
        }

        let method = parts[0].to_string();
        let path = parts[1].to_string();

        let mut headers = Vec::new();
        let mut body = None;

        for line in lines.take_while(|l| !l.is_empty()) {
            if let Some((key, value)) = line.split_once(':') {
                headers.push((key.trim().to_string(), value.trim().to_string()));
            }
        }

        if let Some(body_start) = raw_request.find("\r\n\r\n") {
            body = Some(raw_request[body_start + 4..].to_string());
        }

        Ok(HttpRequest {
            method,
            path,
            headers,
            body,
        })
    }

    fn handle_request(req: &HttpRequest, config: &ServerConfig) -> HttpResponse {
        let compression_scheme = req
            .headers
            .iter()
            .find(|(k, _)| k.to_lowercase() == "accept-encoding")
            .and_then(|(_, v)| CompressionUtil::negotiate_compression(v));

        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => Self::hello_world(compression_scheme),
            ("GET", "/user-agent") => Self::user_agent(req, compression_scheme),
            (_method, path) if path.starts_with("/echo/") => Self::echo(path, compression_scheme),
            (method, path) if path.starts_with("/files/") => {
                let filename = &path[7..];
                match method {
                    "GET" => Self::serve_file(filename, config),
                    "POST" => Self::create_file(filename, req, config),
                    _ => Self::method_not_allowed(),
                }
            }
            _ => Self::not_found(),
        }
    }

    fn hello_world(compression: Option<CompressionScheme>) -> HttpResponse {
        let body = "Hello, World!".as_bytes().to_vec();
        Self::compress_response(body, "text/plain", compression)
    }

    fn user_agent(req: &HttpRequest, compression: Option<CompressionScheme>) -> HttpResponse {
        req.headers
            .iter()
            .find(|(k, _)| k == "User-Agent")
            .map(|(_, v)| {
                let body = v.as_bytes().to_vec();
                Self::compress_response(body, "text/plain", compression)
            })
            .unwrap_or_else(|| HttpResponse::new(400, "Bad Request"))
    }

    fn echo(path: &str, compression: Option<CompressionScheme>) -> HttpResponse {
        let message = &path[6..];
        let body = message.as_bytes().to_vec();
        Self::compress_response(body, "text/plain", compression)
    }

    fn serve_file(filename: &str, config: &ServerConfig) -> HttpResponse {
        let file_path = config.directory.join(filename);

        match fs::read(&file_path) {
            Ok(content) => {
                HttpResponse::new(200, "OK").with_body(content, "application/octet-stream")
            }
            Err(_) => HttpResponse::new(404, "Not Found"),
        }
    }

    fn create_file(filename: &str, req: &HttpRequest, config: &ServerConfig) -> HttpResponse {
        let file_path = config.directory.join(filename);

        if !file_path.exists() {
            fs::File::create(&file_path)
                .expect(format!("Error in creating file {}", file_path.display()).as_str());
        }

        match req.body.as_ref() {
            Some(body) => match fs::write(&file_path, body) {
                Ok(_) => HttpResponse::new(201, "Created"),
                Err(_) => HttpResponse::new(500, "Internal Server Error"),
            },
            None => HttpResponse::new(400, "Bad Request"),
        }
    }

    fn compress_response(
        body: Vec<u8>,
        content_type: &str,
        compression: Option<CompressionScheme>,
    ) -> HttpResponse {
        match compression {
            Some(scheme) => match CompressionUtil::compress(&body, scheme) {
                Ok(compressed_body) => HttpResponse::new(200, "OK")
                    .with_body(compressed_body, content_type)
                    .with_header(
                        "Content-Encoding",
                        CompressionUtil::scheme_to_header(scheme),
                    ),
                Err(_) => HttpResponse::new(200, "OK").with_body(body, content_type),
            },
            None => HttpResponse::new(200, "OK").with_body(body, content_type),
        }
    }

    fn not_found() -> HttpResponse {
        HttpResponse::new(404, "Not Found")
    }

    fn method_not_allowed() -> HttpResponse {
        HttpResponse::new(405, "Method Not Allowed")
    }
}

struct ConnectionHandler;

impl ConnectionHandler {
    fn handle_client(mut stream: TcpStream, config: ServerConfig) -> io::Result<()> {
        let mut buffer = [0; 1024];
        let bytes_read = stream.read(&mut buffer)?;
        let request_str = String::from_utf8_lossy(&buffer[..bytes_read]);

        let request = Router::parse_request(&request_str)?;
        let response = Router::handle_request(&request, &config);

        stream.write_all(&response.to_bytes())?;
        stream.flush()?;

        Ok(())
    }
}

struct HttpServer {
    config: ServerConfig,
}

impl HttpServer {
    fn new(port: u16, directory: PathBuf) -> Self {
        HttpServer {
            config: ServerConfig { port, directory },
        }
    }

    fn run(&self) -> io::Result<()> {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", self.config.port))?;
        println!("Server started on port {}", self.config.port);

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let config = self.config.clone();
                    thread::spawn(move || {
                        if let Err(e) = ConnectionHandler::handle_client(stream, config) {
                            eprintln!("Error handling client: {}", e);
                        }
                    });
                }
                Err(e) => {
                    eprintln!("Error accepting connection: {}", e);
                }
            }
        }

        Ok(())
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let directory = if let Some(pos) = args.iter().position(|arg| arg == "--directory") {
        if args.len() > pos + 1 {
            Path::new(&args[pos + 1]).to_path_buf()
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Missing directory argument after --directory flag",
            ));
        }
    } else {
        env::current_dir()?
    };

    let server = HttpServer::new(4221, directory);
    server.run()
}
