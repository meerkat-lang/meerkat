use std::fmt::{Display};
use std::io;
use std::result;

pub type Result<T> = result::Result<T, Error>;

pub enum Error {
    Message(String),
}