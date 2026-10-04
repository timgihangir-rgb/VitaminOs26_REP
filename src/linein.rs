// src/linein.rs
//
// Чтение строки с клавиатуры. Общее для входа в систему и установщика:
// там и здесь нужно одно и то же - набрать текст, уметь стереть символ
// backspace'ом и не печатать пароль.
//
// Ввод идёт по одному символу через keyboard::read_key, поэтому работает и
// с PS/2, и с USB-клавиатуры одинаково.

use alloc::string::String;
use alloc::vec::Vec;

use crate::vga::Writer;

/// Больше символов не набираем: всё, что длиннее, - опечатка, а не пароль.
pub const LINE_MAX: usize = 64;

/// Читает строку до Enter. При `echo == false` символы не выводятся (пароль),
/// но стирание работает. Возвращает то, что набрано.
pub fn read_line(writer: &mut Writer, echo: bool) -> String {
    read_line_inner(writer, echo).unwrap_or_default()
}

/// Как `read_line`, но Esc на пустой строке отменяет ввод (None).
/// Установщику это нужно, чтобы из вопроса про учётную запись можно было
/// выйти, не заканчивая установку.
pub fn read_line_cancelable(writer: &mut Writer, echo: bool) -> Option<String> {
    read_line_inner(writer, echo)
}

fn read_line_inner(writer: &mut Writer, echo: bool) -> Option<String> {
    let mut out: Vec<u8> = Vec::with_capacity(LINE_MAX);
    loop {
        match crate::keyboard::read_key() {
            crate::keyboard::Key::Enter => {
                writer.write_string("\n");
                break;
            }
            // Esc: на пустой строке - отмена ввода, на набранной - очистка.
            crate::keyboard::Key::Esc => {
                if out.is_empty() {
                    writer.write_string("\n");
                    return None;
                }
                out.clear();
                if echo {
                    for _ in 0..LINE_MAX {
                        writer.write_string("\x08 \x08");
                    }
                }
                continue;
            }
            // 0x08 = Backspace: убираем символ и затираем его на экране.
            crate::keyboard::Key::Char(0x08) => {
                if out.pop().is_some() && echo {
                    writer.write_string("\x08 \x08");
                }
            }
            crate::keyboard::Key::Char(c) => {
                if out.len() < LINE_MAX && is_usable(c) {
                    out.push(c);
                    if echo {
                        writer.write_byte(c);
                    }
                }
            }
            _ => {}
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Печатный ASCII без управляющих символов и пробела: имена и пароли набираются
/// латиницей (кириллица в них не предусмотрена), пробел - завершающий символ
/// строки и путать его с содержимым нельзя.
fn is_usable(c: u8) -> bool {
    (0x21..0x7f).contains(&c)
}

/// Допустимое имя пользователя: латинская буква, дальше буквы, цифры, дефис
/// и подчёркивание. Ограничение намеренное - такие имена нельзя спутать ни с
/// путём, ни с именем устройства, и в строки /etc/passwd они не требуют
/// экранирования.
pub fn valid_user_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > crate::user::NAME_MAX {
        return false;
    }
    if !b[0].is_ascii_alphabetic() {
        return false;
    }
    b.iter()
        .all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}