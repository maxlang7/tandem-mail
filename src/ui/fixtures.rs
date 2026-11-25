use anyhow::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::model::{Account, Label, RemoteMessage};
use crate::store::{Store, now};

pub fn seed_demo(store: &mut Store, accounts: &[Account]) -> Result<()> {
    let samples = [
        (
            "Maya Chen",
            "Revision notes for Thursday",
            "I left two notes on the introduction. Could you take a look before we meet on Thursday? The figures are ready to go.",
        ),
        (
            "Systems Lab",
            "Lab notebook is ready for review",
            "November entries are now in the shared notebook. Please add the final spectra before the end of the week.",
        ),
        (
            "Nora Patel",
            "Re: Dinner after the match?",
            "The table is booked for 7:30. I can meet you at the field and walk over from there.",
        ),
        (
            "Department Seminar",
            "Friday seminar · Radical chemistry",
            "Join us at 3:30 in the Mellon Institute auditorium. Coffee is available in the lobby from 3.",
        ),
        (
            "Evan Brooks",
            "Saturday session notes",
            "Here's the practice set we discussed. The last two questions are good candidates to review together.",
        ),
        (
            "Campus Library",
            "Your hold is ready for pickup",
            "You can pick up the book at the main desk through Monday. Bring your campus card.",
        ),
        (
            "Jess Morgan",
            "Photos from last weekend",
            "I pulled my favorites into an album. There are some good ones from right before sunset.",
        ),
        (
            "Course Staff",
            "Office hours schedule",
            "The schedule for next week is published. Tuesday will be in the usual room, and Friday moves to the third floor.",
        ),
        (
            "Lena Wright",
            "Notes on the first draft",
            "The main argument is clear. Can we check the reference list together after class?",
        ),
        (
            "Jamie Rivera",
            "Coffee next week",
            "Would Wednesday at 10 work? There's a quiet corner table at the place on Craig Street.",
        ),
        (
            "Research Computing",
            "Storage maintenance window",
            "The shared storage will be unavailable on Saturday between 8 and 10 AM. Save any open work before then.",
        ),
        (
            "Alex Kim",
            "December calendar",
            "I can cover the Wednesday slot. Please send me the room number once it's confirmed.",
        ),
        (
            "Sam Ellis",
            "Re: Weekend plans",
            "The forecast looks clear. Let's meet at the trailhead around nine.",
        ),
        (
            "Chemistry Colloquium",
            "Fall poster abstracts",
            "Abstract submissions close next Tuesday. A title and a short paragraph are plenty for the first pass.",
        ),
        (
            "Emery Watts",
            "Can you send the updated problem set?",
            "I found last week's copy. If you have the version with the corrected constants, please send it over.",
        ),
    ];
    for (account_index, account) in accounts.iter().enumerate() {
        store.put_labels(
            &account.id,
            &[
                Label {
                    id: "Label_1".into(),
                    name: if account_index == 0 {
                        "Tutoring"
                    } else {
                        "Research"
                    }
                    .into(),
                    kind: "user".into(),
                },
                Label {
                    id: "Label_2".into(),
                    name: if account_index == 0 {
                        "Receipts"
                    } else {
                        "Coursework"
                    }
                    .into(),
                    kind: "user".into(),
                },
                Label {
                    id: "Label_3".into(),
                    name: if account_index == 0 {
                        "Tutoring/SAT"
                    } else {
                        "Research/Lab"
                    }
                    .into(),
                    kind: "user".into(),
                },
            ],
        )?;
        let entries = samples.iter().enumerate().filter(|(i, _)| i % accounts.len() == account_index)
            .map(|(i, (sender, text, body))| {
                let content = if i == 3 {
                    let newsletter = seminar_newsletter(body);
                    format!(
                        concat!(
                            "Content-Type: multipart/alternative; boundary=seminar\r\n\r\n",
                            "--seminar\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\n{body}\r\n",
                            "--seminar\r\nContent-Type: text/html; charset=UTF-8\r\n\r\n{newsletter}\r\n",
                            "--seminar--\r\n"
                        ),
                        body = body,
                        newsletter = newsletter,
                    )
                } else {
                    format!("Content-Type: text/plain; charset=UTF-8\r\n\r\n{body}\r\n")
                };
                let raw = format!("From: {sender} <sender{i}@example.org>\r\nTo: {}\r\nSubject: {text}\r\nMessage-ID: <sample{i}@example.org>\r\n{content}", account.email);
                let category = match i {
                    3 | 5 | 7 | 10 | 13 => "CATEGORY_UPDATES",
                    6 | 12 => "CATEGORY_SOCIAL",
                    _ => "CATEGORY_PERSONAL",
                };
                let mut tags = vec!["INBOX".into(), category.into()];
                if i % 3 != 2 { tags.push("UNREAD".into()); }
                if matches!(i, 0 | 3 | 9) { tags.push("STARRED".into()); }
                if matches!(i, 1 | 8) { tags.push("IMPORTANT".into()); }
                if i % 2 == 1 { tags.push("Label_1".into()); }
                RemoteMessage {
                    id: format!("msg{i}"), thread_id: format!("thread{i}"),
                    label_ids: tags, internal_date: (now() - (i as i64 + 1) * 6_400_000).to_string(),
                    raw: Some(URL_SAFE_NO_PAD.encode(raw)),
                }
            }).collect();
        store.apply_messages(&account.id, 0, entries, &[])?;
    }
    store.save_search("Unread with attachments", "is:unread has:attachment", None)?;
    Ok(())
}

fn seminar_newsletter(intro: &str) -> String {
    format!(
        r##"<body style="background: #eeeeee">
    <table align="center" width="550" cellspacing="0" cellpadding="20" bgcolor="#ffffff">
      <tr><td style="background: #23378b; color: white"><h1>Department seminar</h1></td></tr>
      <tr><td style="color: #222222">
        <h2>Radical chemistry</h2>
        <p>{intro}</p>
        <h2>About the talk</h2>
        <p>The seminar will cover how changes to a reaction's conditions affect the lifetime
        of radical intermediates, and what those changes can tell us about the mechanism.</p>
        <a href="https://example.org/seminar">Read the seminar abstract</a>
      </td></tr>
    </table>
    </body>"##
    )
}
