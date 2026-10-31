use std::io::{self, BufRead, Write};

use crate::protocol::Frame::BulkString;

const MAX_LINE_LENGTH: usize = 8192;
const MAX_BULK_LENGTH: usize = 1024 * 1024;
const MAX_ARRAY_LENGTH: usize = 1024;
const MAX_DEPTH: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    SimpleString(String),
    Error(String),
    Integer(i64),
    BulkString(Option<Vec<u8>>),
    Array(Option<Vec<Frame>>),
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_line<R: BufRead>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();

    loop {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;
        buf.push(byte[0]);

        if buf.len() > MAX_LINE_LENGTH + 2 {
            return Err(invalid("RESP line too long"));
        }

        if byte[0] == b'\n' {
            if !buf.ends_with(b"\r\n") {
                return Err(invalid("expected CRLF"));
            }

            buf.truncate(buf.len() - 2);
            return Ok(buf);
        }
    }
}

fn read_integer<R: BufRead>(reader: &mut R) -> io::Result<i64> {
    let line = read_line(reader)?;

    let text = std::str::from_utf8(&line).map_err(|_| invalid("invalid integer"))?;

    text.parse::<i64>().map_err(|_| invalid("invalid integer"))
}

fn read_length<R: BufRead>(reader: &mut R, max: usize) -> io::Result<Option<usize>> {
    let length = read_integer(reader)?;

    if length == -1 {
        return Ok(None);
    }

    let length = usize::try_from(length).map_err(|_| invalid("invalid RESP length"))?;
    if length > max {
        return Err(invalid("invalid RESP length"));
    }

    Ok(Some(length))
}

fn parse_frame<R: BufRead>(reader: &mut R, prefix: u8, depth: usize) -> io::Result<Frame> {
    if depth > MAX_DEPTH {
        return Err(invalid("RESP nesting too deep"));
    }

    match prefix {
        b'+' | b'-' => {
            let bytes = read_line(reader)?;
            let text = String::from_utf8(bytes).map_err(|_| invalid("invalid string"))?;

            if prefix == b'+' {
                Ok(Frame::SimpleString(text))
            } else {
                Ok(Frame::Error(text))
            }
        }

        b':' => Ok(Frame::Integer(read_integer(reader)?)),

        b'$' => match read_length(reader, MAX_BULK_LENGTH)? {
            None => Ok(Frame::BulkString(None)),

            Some(length) => {
                let mut data = vec![0u8; length];
                reader.read_exact(&mut data)?;

                let mut crlf = [0u8; 2];
                reader.read_exact(&mut crlf)?;

                if crlf != *b"\r\n" {
                    return Err(invalid("invalid bulk string ending"));
                }

                Ok(Frame::BulkString(Some(data)))
            }
        },

        b'*' => match read_length(reader, MAX_ARRAY_LENGTH)? {
            None => Ok(Frame::Array(None)),

            Some(length) => {
                let mut items = Vec::with_capacity(length);

                for _ in 0..length {
                    let mut next = [0u8; 1];
                    reader.read_exact(&mut next)?;

                    items.push(parse_frame(reader, next[0], depth + 1)?);
                }
                Ok(Frame::Array(Some(items)))
            }
        },
        _ => Err(invalid("unknown RESP prefix")),
    }
}

pub fn read_frame<R: BufRead>(reader: &mut R) -> io::Result<Option<Frame>> {

    let mut prefix = [0u8; 1];

    if reader.read(&mut prefix)? == 0 {
        return Ok(None);
    }

    parse_frame(reader, prefix[0], 0).map(Some)
}

impl Frame {
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Frame::SimpleString(s) => {
                write!(writer, "+{}\r\n", s)
            }
            Frame::Error(s) => {
                write!(writer, "-{}\r\n", s)
            }
            Frame::Integer(n) => {
                write!(writer, ":{}\r\n", n)
            }
            Frame::BulkString(None) => {
                writer.write_all(b"$-1\r\n")
            }
            Frame::BulkString(Some(data)) => {
                write!(writer, "${}\r\n", data.len())?;
                writer.write_all(data)?;
                writer.write_all(b"\r\n")
            }
            Frame::Array(None)=> {
                writer.write_all(b"*-1\r\n")
            }
            Frame::Array(Some(items)) => {
                write!(writer, "*{}\r\n", items.len())?;

                for item in items {
                    item.write_to(writer)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Frame, MAX_ARRAY_LENGTH, MAX_BULK_LENGTH, MAX_DEPTH, MAX_LINE_LENGTH, invalid, parse_frame,
        read_integer, read_length, read_line,
    };
    use std::io::{BufReader, Cursor, ErrorKind};

    // 실제 프레임 표기로 테스트하되, 접두사는 parse_frame에 별도로 전달한다.
    fn parse_test_frame(input: &[u8]) -> std::io::Result<Frame> {
        let (prefix, body) = input.split_first().expect("test input needs a prefix");
        parse_frame(&mut Cursor::new(body), *prefix, 0)
    }

    #[test]
    fn parse_frame_reads_simple_strings_and_errors() {
        for (input, expected) in [
            (b"+OK\r\n".as_slice(), Frame::SimpleString("OK".into())),
            (b"+\r\n".as_slice(), Frame::SimpleString(String::new())),
            (
                b"-ERR unknown command\r\n".as_slice(),
                Frame::Error("ERR unknown command".into()),
            ),
            (b"-\r\n".as_slice(), Frame::Error(String::new())),
        ] {
            assert_eq!(parse_test_frame(input).unwrap(), expected);
        }
    }

    #[test]
    fn parse_frame_reads_integers() {
        for value in [0, 42, -42, i64::MIN, i64::MAX] {
            let input = format!(":{value}\r\n");
            assert_eq!(
                parse_test_frame(input.as_bytes()).unwrap(),
                Frame::Integer(value)
            );
        }
    }

    #[test]
    fn parse_frame_reads_bulk_strings() {
        for (input, expected) in [
            (b"$5\r\nhello\r\n".as_slice(), Some(b"hello".to_vec())),
            (b"$0\r\n\r\n".as_slice(), Some(Vec::new())),
            (b"$-1\r\n".as_slice(), None),
            // Bulk String은 CRLF와 UTF-8이 아닌 바이트도 내용으로 담을 수 있다.
            (
                b"$4\r\n\xff\x00\r\n\r\n".as_slice(),
                Some(vec![0xff, 0, b'\r', b'\n']),
            ),
        ] {
            assert_eq!(
                parse_test_frame(input).unwrap(),
                Frame::BulkString(expected)
            );
        }
    }

    #[test]
    fn parse_frame_distinguishes_null_and_empty_arrays() {
        assert_eq!(parse_test_frame(b"*-1\r\n").unwrap(), Frame::Array(None));
        assert_eq!(
            parse_test_frame(b"*0\r\n").unwrap(),
            Frame::Array(Some(Vec::new()))
        );
    }

    #[test]
    fn parse_frame_reads_mixed_nested_arrays() {
        let input = b"*4\r\n+OK\r\n:42\r\n$3\r\nfoo\r\n*2\r\n-ERR failed\r\n$-1\r\n";
        let expected = Frame::Array(Some(vec![
            Frame::SimpleString("OK".into()),
            Frame::Integer(42),
            Frame::BulkString(Some(b"foo".to_vec())),
            Frame::Array(Some(vec![
                Frame::Error("ERR failed".into()),
                Frame::BulkString(None),
            ])),
        ]));

        assert_eq!(parse_test_frame(input).unwrap(), expected);
    }

    #[test]
    fn parse_frame_preserves_following_frames() {
        let mut reader = Cursor::new(b"3\r\nfoo\r\n:7\r\n");
        assert_eq!(
            parse_frame(&mut reader, b'$', 0).unwrap(),
            Frame::BulkString(Some(b"foo".to_vec()))
        );
        assert_eq!(reader.position(), 8);
        // 다음 프레임의 접두사까지 소비하지 않았는지 확인한다.
        assert_eq!(read_line(&mut reader).unwrap(), b":7");
    }

    #[test]
    fn parse_frame_handles_small_buffer() {
        let mut reader = BufReader::with_capacity(1, Cursor::new(b"2\r\n$4\r\nPING\r\n:7\r\n"));
        assert_eq!(
            parse_frame(&mut reader, b'*', 0).unwrap(),
            Frame::Array(Some(vec![
                Frame::BulkString(Some(b"PING".to_vec())),
                Frame::Integer(7),
            ]))
        );
    }

    #[test]
    fn parse_frame_rejects_invalid_text_and_numbers() {
        for (input, message) in [
            (b"+\xff\r\n".as_slice(), "invalid string"),
            (b"-\xff\r\n".as_slice(), "invalid string"),
            (b":abc\r\n".as_slice(), "invalid integer"),
            (b":9223372036854775808\r\n".as_slice(), "invalid integer"),
            (b"+OK\n".as_slice(), "expected CRLF"),
        ] {
            let error = parse_test_frame(input).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData);
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn parse_frame_rejects_unknown_prefixes() {
        for input in [b"?hello\r\n".as_slice(), b"*1\r\n?hello\r\n".as_slice()] {
            let error = parse_test_frame(input).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData);
            assert_eq!(error.to_string(), "unknown RESP prefix");
        }
    }

    #[test]
    fn parse_frame_rejects_invalid_lengths() {
        for input in [
            "$-2\r\n".to_string(),
            "*-2\r\n".to_string(),
            format!("${}\r\n", MAX_BULK_LENGTH + 1),
            format!("*{}\r\n", MAX_ARRAY_LENGTH + 1),
        ] {
            // 본문이 없어도 읽기 전에 길이 제한으로 거부되어야 한다.
            let error = parse_test_frame(input.as_bytes()).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData);
            assert_eq!(error.to_string(), "invalid RESP length");
        }
    }

    #[test]
    fn parse_frame_accepts_maximum_lengths() {
        let payload = vec![b'a'; MAX_BULK_LENGTH];
        let mut input = format!("${MAX_BULK_LENGTH}\r\n").into_bytes();
        input.extend_from_slice(&payload);
        input.extend_from_slice(b"\r\n");
        assert_eq!(
            parse_test_frame(&input).unwrap(),
            Frame::BulkString(Some(payload))
        );

        let input = format!(
            "*{MAX_ARRAY_LENGTH}\r\n{}",
            ":0\r\n".repeat(MAX_ARRAY_LENGTH)
        );
        let expected = (0..MAX_ARRAY_LENGTH).map(|_| Frame::Integer(0)).collect();
        assert_eq!(
            parse_test_frame(input.as_bytes()).unwrap(),
            Frame::Array(Some(expected))
        );
    }

    #[test]
    fn parse_frame_rejects_invalid_bulk_ending() {
        for input in [b"$3\r\nabcXX".as_slice(), b"$0\r\n\n\r".as_slice()] {
            let error = parse_test_frame(input).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData);
            assert_eq!(error.to_string(), "invalid bulk string ending");
        }
    }

    #[test]
    fn parse_frame_reports_truncated_frames() {
        for input in [
            b"+".as_slice(),
            b"-ERR\r",
            b":",
            b"$",
            b"$3\r\nab",
            b"$3\r\nabc",
            b"$3\r\nabc\r",
            b"*",
            b"*1\r\n",
            b"*2\r\n:1\r\n",
            b"*1\r\n$3\r\nab",
        ] {
            let error = parse_test_frame(input).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::UnexpectedEof, "{input:?}");
        }
    }

    #[test]
    fn parse_frame_checks_nesting_limit() {
        // 루트 depth는 0이며, 자식 프레임마다 1씩 증가한다.
        let input = format!("{}:7\r\n", "*1\r\n".repeat(MAX_DEPTH));
        let mut expected = Frame::Integer(7);
        for _ in 0..MAX_DEPTH {
            expected = Frame::Array(Some(vec![expected]));
        }
        assert_eq!(parse_test_frame(input.as_bytes()).unwrap(), expected);

        let input = format!("{}:7\r\n", "*1\r\n".repeat(MAX_DEPTH + 1));
        let error = parse_test_frame(input.as_bytes()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "RESP nesting too deep");
    }

    #[test]
    fn invalid_test() {
        assert_eq!(invalid("message").kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_line_test() {
        let mut reader = Cursor::new(b"PING\r\n");

        assert_eq!(read_line(&mut reader).unwrap(), b"PING");
    }

    #[test]
    fn read_line_accepts_empty_line() {
        let mut reader = Cursor::new(b"\r\n");

        assert_eq!(read_line(&mut reader).unwrap(), b"");
    }

    #[test]
    fn read_line_preserves_next_line() {
        let mut reader = Cursor::new(b"first\r\nsecond\r\n");

        assert_eq!(read_line(&mut reader).unwrap(), b"first");
        assert_eq!(read_line(&mut reader).unwrap(), b"second");
    }

    #[test]
    fn read_line_preserves_non_utf8_bytes() {
        let mut reader = Cursor::new(b"\xff\x00\r\n");

        assert_eq!(read_line(&mut reader).unwrap(), b"\xff\x00");
    }

    #[test]
    fn read_line_handles_small_buffer() {
        // CR과 LF가 서로 다른 버퍼 채우기에서 읽혀도 정상 처리한다.
        let mut reader = BufReader::with_capacity(1, Cursor::new(b"PING\r\n"));

        assert_eq!(read_line(&mut reader).unwrap(), b"PING");
    }

    #[test]
    fn read_line_rejects_lf_without_cr() {
        for input in [b"PING\n".as_slice(), b"\n".as_slice()] {
            let mut reader = Cursor::new(input);

            let error = read_line(&mut reader).unwrap_err();

            assert_eq!(error.kind(), ErrorKind::InvalidData);
            assert_eq!(error.to_string(), "expected CRLF");
        }
    }

    #[test]
    fn read_line_reports_unexpected_eof() {
        // 빈 입력, 종료 문자 누락, CR 직후 입력 종료를 확인한다.
        for input in [b"".as_slice(), b"PING".as_slice(), b"PING\r".as_slice()] {
            let mut reader = Cursor::new(input);

            let error = read_line(&mut reader).unwrap_err();

            assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
        }
    }

    #[test]
    fn read_line_accepts_maximum_length() {
        // 길이 제한은 끝의 CRLF를 제외한 내용의 길이다.
        let expected = vec![b'a'; MAX_LINE_LENGTH];
        let mut input = expected.clone();
        input.extend_from_slice(b"\r\n");
        let mut reader = Cursor::new(input);

        assert_eq!(read_line(&mut reader).unwrap(), expected);
    }

    #[test]
    fn read_line_rejects_length_above_limit() {
        let mut input = vec![b'a'; MAX_LINE_LENGTH + 1];
        input.extend_from_slice(b"\r\n");
        let mut reader = Cursor::new(input);

        let error = read_line(&mut reader).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "RESP line too long");
    }

    #[test]
    fn read_line_rejects_overlong_input_without_crlf() {
        // 종료 문자가 없어도 전체 버퍼 한도를 넘으면 길이 오류를 낸다.
        let mut reader = Cursor::new(vec![b'a'; MAX_LINE_LENGTH + 3]);

        let error = read_line(&mut reader).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "RESP line too long");
    }

    #[test]
    fn read_integer_test() {
        for (input, expected) in [("42\r\n", 42), ("-42\r\n", -42), ("0\r\n", 0)] {
            let mut reader = Cursor::new(input.as_bytes());

            assert_eq!(read_integer(&mut reader).unwrap(), expected, "{input:?}");
        }
    }

    #[test]
    fn read_integer_accepts_i64_boundaries() {
        for expected in [i64::MIN, i64::MAX] {
            let mut reader = Cursor::new(format!("{expected}\r\n").into_bytes());

            assert_eq!(read_integer(&mut reader).unwrap(), expected);
        }
    }

    #[test]
    fn read_integer_rejects_overflow_and_underflow() {
        for input in ["9223372036854775808\r\n", "-9223372036854775809\r\n"] {
            let mut reader = Cursor::new(input.as_bytes());

            let error = read_integer(&mut reader).unwrap_err();

            assert_eq!(error.kind(), ErrorKind::InvalidData, "{input:?}");
            assert_eq!(error.to_string(), "invalid integer");
        }
    }

    #[test]
    fn read_integer_rejects_invalid_number_text() {
        // 빈 문자열, 부호만 있는 입력, 소수, 공백 등은 정수로 받아들이지 않는다.
        for text in [
            "", "+", "-", "abc", "12a", "1.5", "1e3", " 42", "42 ", "1 2", "--1", "+-1",
        ] {
            let mut reader = Cursor::new(format!("{text}\r\n").into_bytes());

            let error = read_integer(&mut reader).unwrap_err();

            assert_eq!(error.kind(), ErrorKind::InvalidData, "{text:?}");
            assert_eq!(error.to_string(), "invalid integer");
        }
    }

    #[test]
    fn read_integer_rejects_non_utf8_bytes() {
        let mut reader = Cursor::new(b"\xff\r\n");

        let error = read_integer(&mut reader).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "invalid integer");
    }

    #[test]
    fn read_integer_preserves_next_line() {
        let mut reader = Cursor::new(b"42\r\n-7\r\n");

        assert_eq!(read_integer(&mut reader).unwrap(), 42);
        assert_eq!(read_integer(&mut reader).unwrap(), -7);
    }

    #[test]
    fn read_integer_handles_small_buffer() {
        let mut reader = BufReader::with_capacity(1, Cursor::new(b"-123\r\n"));

        assert_eq!(read_integer(&mut reader).unwrap(), -123);
    }

    #[test]
    fn read_integer_propagates_invalid_line_ending() {
        let mut reader = Cursor::new(b"42\n");

        let error = read_integer(&mut reader).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "expected CRLF");
    }

    #[test]
    fn read_integer_propagates_unexpected_eof() {
        for input in [b"".as_slice(), b"42".as_slice(), b"42\r".as_slice()] {
            let mut reader = Cursor::new(input);

            let error = read_integer(&mut reader).unwrap_err();

            assert_eq!(error.kind(), ErrorKind::UnexpectedEof, "{input:?}");
        }
    }

    #[test]
    fn read_integer_propagates_line_length_limit() {
        let mut input = vec![b'1'; MAX_LINE_LENGTH + 1];
        input.extend_from_slice(b"\r\n");
        let mut reader = Cursor::new(input);

        let error = read_integer(&mut reader).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "RESP line too long");
    }

    #[test]
    fn read_length_accepts_null() {
        // -1은 길이 0과 달리 값이 없음을 뜻한다. max가 0이어도 허용한다.
        for max in [0, MAX_ARRAY_LENGTH, MAX_BULK_LENGTH] {
            let mut reader = Cursor::new(b"-1\r\n");
            assert_eq!(read_length(&mut reader, max).unwrap(), None);
        }
    }

    #[test]
    fn read_length_accepts_zero_and_positive_lengths() {
        for length in [0, 1, 42] {
            let mut reader = Cursor::new(format!("{length}\r\n").into_bytes());
            assert_eq!(read_length(&mut reader, 42).unwrap(), Some(length));
        }
    }

    #[test]
    fn read_length_checks_configured_boundaries() {
        for max in [0, MAX_ARRAY_LENGTH, MAX_BULK_LENGTH] {
            let mut reader = Cursor::new(format!("{max}\r\n").into_bytes());
            assert_eq!(read_length(&mut reader, max).unwrap(), Some(max));

            let mut reader = Cursor::new(format!("{}\r\n", max + 1).into_bytes());
            let error = read_length(&mut reader, max).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData);
        }
    }

    #[test]
    fn read_length_rejects_negative_values_other_than_null() {
        for length in [-2, -42, i64::MIN] {
            let mut reader = Cursor::new(format!("{length}\r\n").into_bytes());
            let error = read_length(&mut reader, MAX_BULK_LENGTH).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData, "{length}");
        }
    }

    #[test]
    fn read_length_propagates_invalid_integer() {
        for input in [
            b"\r\n".as_slice(),
            b"abc\r\n".as_slice(),
            b"1.5\r\n".as_slice(),
            b"\xff\r\n".as_slice(),
            b"9223372036854775808\r\n".as_slice(),
        ] {
            let mut reader = Cursor::new(input);
            let error = read_length(&mut reader, MAX_BULK_LENGTH).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData);
            assert_eq!(error.to_string(), "invalid integer");
        }
    }

    #[test]
    fn read_length_propagates_invalid_line_ending() {
        let mut reader = Cursor::new(b"3\n");
        let error = read_length(&mut reader, 10).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "expected CRLF");
    }

    #[test]
    fn read_length_propagates_unexpected_eof() {
        for input in [b"".as_slice(), b"3".as_slice(), b"3\r".as_slice()] {
            let mut reader = Cursor::new(input);
            let error = read_length(&mut reader, 10).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
        }
    }

    #[test]
    fn read_length_leaves_payload_unread() {
        let mut reader = Cursor::new(b"3\r\nabc\r\n");

        assert_eq!(read_length(&mut reader, 10).unwrap(), Some(3));
        assert_eq!(read_line(&mut reader).unwrap(), b"abc");
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn read_length_accepts_max_above_i64_range() {
        // max를 i64로 변환하면 음수가 되는 경우에도 유효한 길이를 허용한다.
        for length in [0, 42, i64::MAX] {
            let mut reader = Cursor::new(format!("{length}\r\n").into_bytes());
            assert_eq!(
                read_length(&mut reader, usize::MAX).unwrap(),
                Some(length as usize)
            );
        }
    }
}
