//! Manages ZofiDesk user accounts in the database hbbs uses (`ZOFI_DB`, default `zofidesk.sqlite3`).

use hbb_common::{bail, tokio, ResultType};
use hbbs::zofi::{db_path, store::Db};
use std::io::BufRead;

const USAGE: &str = "Usage:
  zofidesk-admin user add <username> [password] [--admin]
  zofidesk-admin user list
  zofidesk-admin user passwd <username> [password]
  zofidesk-admin user disable <username>
  zofidesk-admin user enable <username>

When the password is omitted it is read from standard input, so it stays out of the shell history.";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(err) = run(&args).await {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

async fn run(args: &[String]) -> ResultType<()> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let db = Db::open(&db_path()).await?;
    match args.as_slice() {
        ["user", "add", username, rest @ ..] => {
            let is_admin = rest.contains(&"--admin");
            let password = password_arg(rest)?;
            db.add_user(username, &password, is_admin).await?;
            println!("User {username} created");
        }
        ["user", "list"] => {
            for user in db.list_users().await? {
                println!(
                    "{:<30} {:<8} {}",
                    user.username,
                    if user.active { "active" } else { "disabled" },
                    if user.is_admin { "admin" } else { "" }
                );
            }
        }
        ["user", "passwd", username, rest @ ..] => {
            let password = password_arg(rest)?;
            db.set_password(username, &password).await?;
            println!("Password changed; {username} was signed out everywhere");
        }
        ["user", "disable", username] => {
            db.set_active(username, false).await?;
            println!("User {username} disabled and signed out everywhere");
        }
        ["user", "enable", username] => {
            db.set_active(username, true).await?;
            println!("User {username} enabled");
        }
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

fn password_arg(rest: &[&str]) -> ResultType<String> {
    if let Some(password) = rest.iter().find(|arg| !arg.starts_with("--")) {
        return Ok((*password).to_owned());
    }
    eprint!("Password: ");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}
