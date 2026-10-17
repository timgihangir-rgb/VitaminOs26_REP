// src/login.rs
//
// Вход в систему при загрузке: имя пользователя и пароль, без права сдачи
// система шеллу.
//
// Пока в /etc/passwd нет ни одной учётной записи (загрузка с live-образа или
// свежий диск, минуя установщик), вход не спрашивается: иначе систему было бы
// не запустить вовсе. Настоящую защиту даёт только установленная копия, у
// которой учётная запись создаёт установщик.
//
// root за консолью не пускаем - он без пароля и существует только для
// внутренних нужд (установщик, фоновые службы) и для sudo. Если пароль
// администратора забыт, путь восстановления один: загрузиться с live-образа
// и починить /etc/passwd вручную.

use crate::linein::read_line;
use crate::user::{self, Account};
use crate::vga::{Writer, COLOR_DARK_GREY, COLOR_LIGHT_GREEN, COLOR_LIGHT_RED, COLOR_WHITE};

/// Сколько попыток входа подряд.
const ATTEMPTS: usize = 3;

/// Итог входа в систему.
pub enum Outcome {
    /// Вход состоялся, работаем под этой учётной записью.
    LoggedIn(Account),
    /// Учётных записей на ФС нет (live-образ, диск мимо установщика):
    /// пароль не спрашиваем, работаем от root.
    NoAccounts,
    /// Попытки исчерпаны. Шелл запускать нельзя - иначе пароль оказался бы
    /// не защитой, а формальностью.
    Failed,
}

/// Просит логин и пароль и переводит систему на этого пользователя.
pub fn login(writer: &mut Writer) -> Outcome {
    crate::vga::serial_write_atomic("[L] login prompt\n");
    if user::all().is_empty() {
        writer.set_color(COLOR_DARK_GREY);
        writer.write_string(
            "\nno user accounts on this filesystem - running without login\n\n",
        );
        writer.set_color(COLOR_WHITE);
        user::set_current_uid(user::ROOT_UID);
        return Outcome::NoAccounts;
    }

    writer.write_string("\n");
    for attempt in 1..=ATTEMPTS {
        writer.set_color(COLOR_WHITE);
        write_prompt(writer, "VitaminOS26 login");
        writer.write_string("  login: ");
        writer.sync();
        let name = read_line(writer, true);

        if name.is_empty() {
            // Пустая строка - просто следующая попытка, без сообщения.
            continue;
        }

        if name == "root" {
            writer.set_color(COLOR_LIGHT_RED);
            writer.write_string("root may not log in on the console; use sudo\n");
            writer.set_color(COLOR_WHITE);
            writer.sync();
            continue;
        }

        writer.write_string("  password: ");
        let password = read_line(writer, false);

        let ok = user::verify(&name, &password);
        crate::vga::serial_write_atomic(if ok {
            "[L] auth OK\n"
        } else {
            "[L] auth FAILED\n"
        });
        if ok {
            let account = match user::lookup(&name) {
                Some(a) => a,
                None => {
                    // Хеш есть, а записи в passwd нет - запись битая.
                    writer.set_color(COLOR_LIGHT_RED);
                    writer.write_string("account record is broken\n");
                    writer.set_color(COLOR_WHITE);
                    writer.sync();
                    continue;
                }
            };
            user::set_current_uid(account.uid);
            writer.set_color(COLOR_LIGHT_GREEN);
            writer.write_string("\nWelcome, ");
            writer.write_string(&account.name);
            writer.write_string(".\n\n");
            writer.set_color(COLOR_WHITE);
            writer.sync();
            return Outcome::LoggedIn(account);
        }

        writer.set_color(COLOR_LIGHT_RED);
        writer.write_string("Login incorrect\n\n");
        writer.set_color(COLOR_WHITE);

        if attempt == ATTEMPTS {
            writer.set_color(COLOR_LIGHT_RED);
            writer.write_string("Too many attempts\n");
            writer.set_color(COLOR_WHITE);
        }
        writer.sync();
    }
    Outcome::Failed
}

fn write_prompt(writer: &mut Writer, text: &str) {
    let pad = crate::vga::screen_cols().saturating_sub(text.len()) / 2;
    for _ in 0..pad {
        writer.write_string(" ");
    }
    writer.write_string(text);
    writer.write_string("\n\n");
}