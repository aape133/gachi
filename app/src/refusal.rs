//! Refusals from the connection server and from a room's host, in the user's language.
//!
//! Both say no in Korean: the server always has, and a host keeps writing what older
//! versions show as is. The app here knows those sentences and says them in its own
//! language; one it does not know is said by its kind (or as written, in Korean).

use gachi_lang::tr;
use gachi_proto::signal::ErrorKind;

/// A refusal, as this user reads it.
pub fn localize(kind: ErrorKind, message: &str) -> String {
    let m = message.trim();
    if let Some(s) = known(m) {
        return s;
    }
    if gachi_lang::get() == "ko-KR" && !m.is_empty() {
        return m.to_string();
    }
    match kind {
        ErrorKind::NotFound => tr!("refusal.room_not_found"),
        ErrorKind::Full => tr!("refusal.room_full"),
        ErrorKind::PasswordRequired => tr!("app.room_has_password"),
        ErrorKind::WrongPassword => tr!("app.wrong_password"),
        ErrorKind::RateLimited => tr!("refusal.please_try_again"),
        ErrorKind::CodeTaken => tr!("refusal.another_room_using"),
        ErrorKind::Timeout => tr!("refusal.host_didnt_answer"),
        ErrorKind::BadRequest => tr!("refusal.invalid_request"),
    }
}

/// The sentences the server and hosts send (Korean), in the current language.
fn known(m: &str) -> Option<String> {
    Some(match m {
        // the connection server (worker/src/index.ts)
        "잠시 후 다시 해 주세요." => tr!("refusal.please_try_again"),
        "시도가 너무 많아요. 잠시 후 다시 해 주세요." => tr!("refusal.too_many_attempts"),
        "비밀번호가 여러 번 틀렸어요. 10분 뒤에 다시 해 주세요." => tr!("refusal.too_many_wrong"),
        "방을 찾을 수 없어요. 코드를 다시 확인해 주세요." => tr!("refusal.room_not_found"),
        "방을 찾을 수 없어요. 코드를 다시 확인해 주세요. 방장 연결이 잠시 끊긴 것이라면 조금 뒤에 다시 시도해 주세요." => tr!("refusal.room_not_found_please"),
        "코드 형식이 맞지 않아요." | "코드 형식이 맞지 않아요" => tr!("refusal.doesnt_look_like"),
        "지금 들어오려는 사람이 너무 많아요. 잠시 후 다시 해 주세요." => tr!("refusal.too_many_people"),
        "방장이 응답하지 않았어요. 다시 시도해 주세요." => tr!("refusal.host_didnt_answer"),
        "이 코드는 다른 방이 쓰고 있어요. 새 코드를 만들어 주세요." => tr!("refusal.another_room_using_code"),
        "다른 기기에서 방을 이어받았어요." => tr!("refusal.room_taken_over"),
        // the host (session.rs, `admit`)
        "방을 찾을 수 없어요." => tr!("refusal.room_not_found_2"),
        "방이 가득 찼어요." => tr!("refusal.room_full"),
        "비밀번호가 있는 방이에요." => tr!("app.room_has_password"),
        "비밀번호가 여러 번 틀렸어요. 1분 뒤에 다시 시도해 주세요." => tr!("refusal.too_many_wrong_passwords"),
        "잘못된 요청이에요." => tr!("refusal.invalid_request"),
        "방장이 아직 준비 중이에요. 다시 시도해 주세요." => tr!("refusal.host_still_getting"),
        "비밀번호가 맞지 않아요." => tr!("app.wrong_password"),
        "이 방은 8명보다 크게 열려 있어서 gachi 새 버전이 필요해요. 업데이트한 뒤 다시 들어와 주세요." => tr!("refusal.room_open_more"),
        "이 방에는 들어갈 수 없어요." => tr!("refusal.cant_join_room"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gachi_lang::with;

    #[test]
    fn known_refusals_are_said_in_the_users_language() {
        let m = "방이 가득 찼어요.";
        assert_eq!(with("ko-KR", || localize(ErrorKind::Full, m)), m);
        assert_eq!(
            with("en-US", || localize(ErrorKind::Full, m)),
            "The room is full."
        );
        assert_eq!(
            with("ja-JP", || localize(ErrorKind::Full, m)),
            "ルームが満員です。"
        );
    }

    #[test]
    fn unknown_ones_are_kept_in_korean_or_said_by_their_kind() {
        let m = "새로운 거절 문구";
        assert_eq!(with("ko-KR", || localize(ErrorKind::NotFound, m)), m);
        assert!(with("en-US", || localize(ErrorKind::NotFound, m)).starts_with("Room not found"));
        assert!(with("ja-JP", || localize(ErrorKind::RateLimited, "")).starts_with("しばらく"));
    }
}
