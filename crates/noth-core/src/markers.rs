//! Noth 以外の Bot の目印のまとめ。Bot の応酬が長いと目印だけで Layer 3 と router の入力を埋めるため
use jiff::Timestamp;

use crate::store::MessageRow;
use crate::types::*;

/// 1 回の判定に付く目印の上限(連続をまとめた後の数)
pub const MAX_MARKER_RUNS: usize = 20;

/// 同じチャンネルで同じ Bot が、間に他の発言を挟まず続けた目印
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerRun {
    pub channel_id: ChannelId,
    pub channel_name: String,
    pub user_id: UserId,
    pub author_name: String,
    /// 1 件のときだけ持つ。複数件の宛先はまとめられない
    pub reply_to_author: Option<String>,
    pub count: usize,
    pub chars: usize,
    pub first_message_id: MessageId,
    pub last_message_id: MessageId,
    pub at: Timestamp,
    /// 次の目印が隣り合うかの判定用。行 ID が飛んでいれば間に他の発言(取り除かれたトリガーを含む)がある
    last_row_id: i64,
}

impl MarkerRun {
    /// Layer 3 / Layer 4 の発言行に入れる文字列
    pub fn render(&self) -> String {
        if self.count == 1 {
            return BotMarker {
                author_name: self.author_name.clone(),
                reply_to_author: self.reply_to_author.clone(),
                chars: self.chars,
            }
            .render(self.first_message_id);
        }
        format!(
            "[Bot {} が {} 件発言(約{}字) 最初の message_id: {} 最後の message_id: {}]",
            self.author_name,
            self.count,
            approx_chars(self.chars),
            self.first_message_id,
            self.last_message_id,
        )
    }
}

#[derive(Debug, Clone)]
pub enum Folded<'a> {
    Row(&'a MessageRow),
    Markers(MarkerRun),
    /// 上限を超えて落とした古い目印。位置は最初に落とした目印があった場所
    Omitted {
        count: usize,
        channel_name: String,
        at: Timestamp,
    },
}

impl Folded<'_> {
    pub fn render_omitted(count: usize) -> String {
        format!("[Bot の発言 {count} 件を省略]")
    }
}

/// 連続した目印を 1 つにまとめ、まとめた後の目印が `max_runs` を超えたら古い方から落とす
pub fn fold(rows: &[MessageRow], max_runs: usize) -> Vec<Folded<'_>> {
    let mut items: Vec<Folded> = Vec::new();
    for row in rows {
        let Some(marker) = &row.bot_marker else {
            items.push(Folded::Row(row));
            continue;
        };
        if let Some(Folded::Markers(run)) = items.last_mut()
            && run.channel_id == row.channel_id
            && run.user_id == row.user_id
            && run.last_row_id + 1 == row.id
        {
            run.count += 1;
            run.chars += marker.chars;
            run.last_message_id = row.message_id;
            run.at = row.created_at;
            run.last_row_id = row.id;
            run.reply_to_author = None;
            continue;
        }
        items.push(Folded::Markers(MarkerRun {
            channel_id: row.channel_id,
            channel_name: row.channel_name.clone(),
            user_id: row.user_id,
            author_name: marker.author_name.clone(),
            reply_to_author: marker.reply_to_author.clone(),
            count: 1,
            chars: marker.chars,
            first_message_id: row.message_id,
            last_message_id: row.message_id,
            at: row.created_at,
            last_row_id: row.id,
        }));
    }

    let runs = items
        .iter()
        .filter(|i| matches!(i, Folded::Markers(_)))
        .count();
    let mut to_drop = runs.saturating_sub(max_runs);
    if to_drop == 0 {
        return items;
    }
    let mut out = Vec::with_capacity(items.len());
    let mut omitted: Option<usize> = None;
    let mut dropped = 0;
    for item in items {
        match item {
            Folded::Markers(run) if to_drop > 0 => {
                to_drop -= 1;
                dropped += run.count;
                if omitted.is_none() {
                    omitted = Some(out.len());
                    out.push(Folded::Omitted {
                        count: 0,
                        channel_name: run.channel_name,
                        at: run.at,
                    });
                }
            }
            other => out.push(other),
        }
    }
    if let Some(i) = omitted
        && let Folded::Omitted { count, .. } = &mut out[i]
    {
        *count = dropped;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, channel_id: ChannelId, user_id: UserId, marker: Option<usize>) -> MessageRow {
        MessageRow {
            id,
            guild_id: 1,
            channel_id,
            channel_name: "general".into(),
            message_id: id as u64 + 1000,
            user_id,
            author_name: format!("u{user_id}"),
            is_self: false,
            mentions_self: false,
            reply_to_self: false,
            reply_to: None,
            content: if marker.is_some() {
                String::new()
            } else {
                "hi".into()
            },
            created_at: Timestamp::UNIX_EPOCH,
            chunk_id: None,
            is_noise: None,
            decision_id: None,
            promoted_turn_id: None,
            bot_marker: marker.map(|chars| BotMarker {
                author_name: format!("u{user_id}"),
                reply_to_author: Some("alice".into()),
                chars,
            }),
        }
    }

    #[test]
    fn consecutive_markers_of_one_bot_in_one_channel_become_one() {
        let rows = vec![
            row(1, 1, 9, Some(1000)),
            row(2, 1, 9, Some(200)),
            row(3, 1, 9, Some(7200)),
            row(4, 1, 5, None),
            row(5, 1, 9, Some(50)),
            row(6, 2, 9, Some(50)),
        ];
        let folded = fold(&rows, MAX_MARKER_RUNS);
        assert_eq!(
            folded.len(),
            4,
            "目印 3 件、人の発言を挟んで 1 件、別チャンネル 1 件"
        );
        let Folded::Markers(run) = &folded[0] else {
            panic!("先頭は目印のまとまり");
        };
        assert_eq!((run.count, run.chars), (3, 8400));
        assert_eq!((run.first_message_id, run.last_message_id), (1001, 1003));
        assert_eq!(
            run.render(),
            "[Bot u9 が 3 件発言(約8,400字) 最初の message_id: 1001 最後の message_id: 1003]"
        );
        let Folded::Markers(single) = &folded[2] else {
            panic!("単独の目印");
        };
        assert!(single.render().contains("alice への返信"));
    }

    #[test]
    fn markers_are_not_folded_across_a_removed_row() {
        // id 2 はトリガーとして取り除かれた人間の発言
        let rows = vec![row(1, 1, 9, Some(10)), row(3, 1, 9, Some(10))];
        let folded = fold(&rows, MAX_MARKER_RUNS);
        assert_eq!(folded.len(), 2);
        assert!(
            folded
                .iter()
                .all(|f| matches!(f, Folded::Markers(r) if r.count == 1))
        );
    }

    #[test]
    fn runs_beyond_the_cap_are_dropped_oldest_first_with_one_omitted_line() {
        // 人の発言を挟んで目印を 25 個作る
        let mut rows = Vec::new();
        for i in 0..25 {
            rows.push(row(i * 2 + 1, 1, 9, Some(10)));
            rows.push(row(i * 2 + 2, 1, 5, None));
        }
        let folded = fold(&rows, MAX_MARKER_RUNS);
        let runs = folded
            .iter()
            .filter(|f| matches!(f, Folded::Markers(_)))
            .count();
        assert_eq!(runs, 20);
        let omitted: Vec<_> = folded
            .iter()
            .filter(|f| matches!(f, Folded::Omitted { .. }))
            .collect();
        assert_eq!(omitted.len(), 1);
        assert!(matches!(folded[0], Folded::Omitted { count: 5, .. }));
        // 落ちたのは古い 5 個で、新しい方は残る
        let Folded::Markers(first_kept) = folded
            .iter()
            .find(|f| matches!(f, Folded::Markers(_)))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(first_kept.first_message_id, 1000 + 11);
        assert_eq!(Folded::render_omitted(5), "[Bot の発言 5 件を省略]");
    }
}
