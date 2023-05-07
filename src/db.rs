use crate::{config::*, reddit::*, types::*};
use anyhow::{Context, Result};
use rusqlite::{named_params, Connection, Row};
use rusqlite::{
    types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, Value, ValueRef},
    OptionalExtension,
};
use rusqlite_migration::{Migrations, M};
use std::convert::TryFrom;
use std::path::Path;
use std::str::FromStr;
use std::string::ToString;

const MIGRATIONS: &[&str] = &[
    "
    create table post(
        post_id     text not null,
        chat_id     integer not null,
        subreddit   text not null,
        seen_at     text not null,
        primary key (post_id, chat_id)
    ) strict;
    ",
    "
    create table subscription(
        chat_id     integer not null,
        subreddit   text not null,
        created_at  text not null,
        post_limit  integer,
        time        text,
        filter      text,
        primary key (subreddit, chat_id)
    ) strict;
    ",
    "
    create table chat(
        chat_id     integer primary key,
        repost_channel_id integer
    ) strict;
    ",
    "
    insert or ignore into chat (chat_id)
    select chat_id from subscription;
    ",
    "
    create table subscription_new(
        chat_id     integer not null,
        subreddit   text not null,
        created_at  text not null,
        post_limit  integer,
        time        text,
        filter      text,
        primary key (subreddit, chat_id),
        foreign key (chat_id) references chat(chat_id)
    );
    ",
    "
    insert into subscription_new
    select * from subscription;
    ",
    "
    drop table subscription;
    ",
    "
    alter table subscription_new
    rename to subscription;
    ",
    "
    create table post_new(
        post_id     text not null,
        chat_id     integer not null,
        subreddit   text not null,
        seen_at     text, -- make seen_at nullable
        post_title  text not null, -- new field
        primary key (post_id, chat_id)
    ) strict;
    ",
    "
    insert into post_new (post_id, chat_id, subreddit, seen_at, post_title)
    select post_id, chat_id, subreddit, seen_at, 'Unknown' as post_title from post;
    ",
    "
    drop table post;
    ",
    "
    alter table post_new
    rename to post;
    ",
];

#[derive(Debug)]
pub struct Database {
    pub conn: Connection,
}

impl Database {
    pub fn open(config: &Config) -> Result<Self> {
        let conn = Self::get_conn(&config.db_path).context("error connecting to database")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Database { conn })
    }

    #[cfg(test)]
    fn get_conn(_db_path: &Path) -> Result<Connection, rusqlite::Error> {
        Connection::open_in_memory()
    }

    #[cfg(not(test))]
    fn get_conn(db_path: &Path) -> Result<Connection, rusqlite::Error> {
        std::fs::create_dir_all(db_path.parent().expect("Db path doesn't contain a file"))
            .expect("Couldn't create directory for db file");
        Connection::open(db_path)
    }

    pub fn migrate(&mut self) -> Result<(), rusqlite_migration::Error> {
        let migrations = MIGRATIONS.iter().map(|e| M::up(e)).collect();
        Migrations::new(migrations).to_latest(&mut self.conn)
    }

    pub fn record_post(
        &self,
        chat_id: i64,
        post: &Post,
        seen_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<()> {
        // First, attempt to insert a new row with INSERT OR IGNORE
        let mut stmt = self.conn.prepare(
            "
            insert or ignore into post (post_id, chat_id, subreddit, seen_at, post_title)
            values (:post_id, :chat_id, :subreddit, :seen_at, :post_title)
            ",
        )?;
        stmt.execute(named_params! {
            ":post_id": post.id,
            ":chat_id": chat_id,
            ":subreddit": &post.subreddit,
            ":seen_at": seen_at,
            ":post_title": &post.title,
        })?;

        // Then, update the seen_at field for the row with the given post_id and chat_id, only if seen_at is null
        let mut stmt = self.conn.prepare(
            "
            update post
            set seen_at = :seen_at
            where post_id = :post_id and chat_id = :chat_id and seen_at is null
            ",
        )?;
        stmt.execute(named_params! {
            ":seen_at": seen_at,
            ":post_id": post.id,
            ":chat_id": chat_id,
        })
        .context("could not update seen_at")
        .map(|_| ())
    }

    pub fn record_post_seen_with_current_time(&self, chat_id: i64, post: &Post) -> Result<()> {
        let current_time = Some(chrono::Utc::now());
        self.record_post(chat_id, post, current_time)
    }

    pub fn get_post_title(&self, chat_id: i64, post_id: &str) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "
            select post_title
            from post
            where post_id = :post_id and chat_id = :chat_id
            ",
        )?;

        let post_title = stmt
            .query_row(
                named_params! {
                    ":post_id": post_id,
                    ":chat_id": chat_id,
                },
                |row| row.get("post_title"),
            )
            .context("could not retrieve post title")?;

        Ok(post_title)
    }

    pub fn is_post_seen(&self, chat_id: i64, post: &Post) -> Result<bool> {
        let mut stmt = self.conn.prepare(
            "
            select exists(
                select 1 
                  from post
                 where post_id = :post_id and chat_id = :chat_id and seen_at is not null
            );
            ",
        )?;

        stmt.query_row(
            named_params! {
                ":post_id": post.id,
                ":chat_id": chat_id
            },
            |row| row.get(0),
        )
        .map_err(anyhow::Error::from)
    }

    pub fn existing_posts_for_subreddit(&self, chat_id: i64, subreddit: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare(
            "
            select exists(
                select 1
                  from post
                 where chat_id = :chat_id and subreddit = :subreddit
            );
            ",
        )?;

        stmt.query_row(
            named_params! {
                ":chat_id": chat_id,
                ":subreddit": subreddit,
            },
            |row| row.get(0),
        )
        .map_err(anyhow::Error::from)
    }

    pub fn subscribe(&self, chat_id: i64, args: &SubscriptionArgs) -> Result<()> {
        self.ensure_chat_exists(chat_id)?;

        let mut stmt = self.conn.prepare(
            "
            insert into subscription (chat_id, subreddit, post_limit, time, filter, created_at)
            values (:chat_id, :subreddit, :limit, :time, :filter, :created_at)
            ",
        )?;
        stmt.execute(named_params! {
            ":chat_id": chat_id,
            ":subreddit": args.subreddit,
            ":limit": args.limit,
            ":time": args.time,
            ":filter": args.filter,
            ":created_at": chrono::Utc::now()
        })
        .context("could not add subscription")?;
        Ok(())
    }

    pub fn unsubscribe(&self, chat_id: i64, subreddit: &str) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "
            delete from subscription
            where chat_id = :chat_id and subreddit LIKE :subreddit
            returning subreddit
            ",
        )?;
        let deleted_subreddit: String = stmt
            .query_row(
                named_params! {
                    ":chat_id": chat_id,
                    ":subreddit": subreddit,
                },
                |row| row.get("subreddit"),
            )
            .context("could not delete subscription")?;

        // Delete posts so that if subreddit is subscribed to later, the first posts seen won't be
        // considered new.
        let mut stmt = self.conn.prepare(
            "
            delete from post
            where chat_id = :chat_id and subreddit = :subreddit
            ",
        )?;
        stmt.execute(named_params! {
            ":chat_id": chat_id,
            ":subreddit": deleted_subreddit,
        })
        .context("could not delete posts")?;

        Ok(deleted_subreddit)
    }

    pub fn get_subscriptions_for_chat(&self, chat_id: i64) -> Result<Vec<Subscription>> {
        let mut stmt = self.conn.prepare(
            "
            select chat_id, subreddit, post_limit, time, filter, created_at
            from subscription
            where chat_id = ?
            ",
        )?;

        let subs = stmt
            .query_map([chat_id], |row| Subscription::try_from(row))?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;

        Ok(subs)
    }

    pub fn get_all_subscriptions(&self) -> Result<Vec<Subscription>> {
        let mut stmt = self.conn.prepare(
            "
            select chat_id, subreddit, post_limit, time, filter, created_at
            from subscription
            ",
        )?;

        let subs = stmt
            .query_map([], |row| Subscription::try_from(row))?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;

        Ok(subs)
    }

    pub fn ensure_chat_exists(&self, chat_id: i64) -> Result<()> {
        let chat_exists: bool = self.conn.query_row(
            "
            select exists(
                select 1
                from chat
                where chat_id = :chat_id
            );
            ",
            named_params! {
                ":chat_id": chat_id,
            },
            |row| row.get(0),
        )?;

        if !chat_exists {
            let mut stmt = self.conn.prepare(
                "
                insert into chat (chat_id)
                values (:chat_id);
                ",
            )?;

            stmt.execute(named_params! {
                ":chat_id": chat_id,
            })
            .context("could not create chat")?;
        }

        Ok(())
    }

    pub fn set_repost_channel(&self, chat_id: i64, repost_channel_id: i64) -> Result<()> {
        self.ensure_chat_exists(chat_id)?;
        let mut stmt = self.conn.prepare(
            "
            update chat
            set repost_channel_id = :repost_channel_id
            where chat_id = :chat_id;
            ",
        )?;

        stmt.execute(named_params! {
            ":chat_id": chat_id,
            ":repost_channel_id": repost_channel_id,
        })
        .context("could not set repost channel")?;

        Ok(())
    }

    pub fn get_repost_channel(&self, chat_id: i64) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare(
            "
            select repost_channel_id
            from chat
            where chat_id = :chat_id;
            ",
        )?;

        let repost_channel_id: Option<i64> = stmt
            .query_row(
                named_params! {
                    ":chat_id": chat_id,
                },
                |row| row.get("repost_channel_id"),
            )
            .optional()
            .context("could not get repost channel")?;

        Ok(repost_channel_id)
    }
}

impl ToSql for TopPostsTimePeriod {
    fn to_sql(&self) -> Result<rusqlite::types::ToSqlOutput, rusqlite::Error> {
        Ok(ToSqlOutput::Owned(Value::Text(self.to_string())))
    }
}

impl ToSql for PostType {
    fn to_sql(&self) -> Result<rusqlite::types::ToSqlOutput, rusqlite::Error> {
        Ok(ToSqlOutput::Owned(Value::Text(self.to_string())))
    }
}

impl FromSql for TopPostsTimePeriod {
    fn column_result(value: ValueRef) -> FromSqlResult<TopPostsTimePeriod> {
        let str = String::column_result(value)?;
        TopPostsTimePeriod::from_str(&str).map_err(|e| FromSqlError::Other(From::from(e)))
    }
}

impl FromSql for PostType {
    fn column_result(value: ValueRef) -> FromSqlResult<PostType> {
        let str = String::column_result(value)?;
        PostType::from_str(&str).map_err(|e| FromSqlError::Other(From::from(e)))
    }
}

impl TryFrom<&Row<'_>> for Subscription {
    type Error = rusqlite::Error;

    fn try_from(row: &Row<'_>) -> Result<Self, Self::Error> {
        Ok(Self {
            subreddit: row.get_unwrap("subreddit"),
            chat_id: row.get_unwrap("chat_id"),
            limit: row.get_unwrap("post_limit"),
            time: row.get_unwrap("time"),
            filter: row.get_unwrap("filter"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reddit::PostType;

    #[test]
    fn test_db() {
        let config = Config::default();
        let mut db = Database::open(&config).unwrap();
        db.migrate().unwrap();
        let post = Post {
            id: "v6nu75".into(),
            created: 1654581100.0,
            post_hint: Some("link".into()),
            subreddit: "absoluteunit".into(),
            title: "Tipping a cow to trim its hooves".into(),
            is_self: false,
            is_video: false,
            is_gallery: Some(false),
            gallery_data: None,
            media_metadata: None,
            ups: 469,
            permalink: "/r/absoluteunit/comments/v6nu75/tipping_a_cow_to_trim_its_hooves/".into(),
            url: "https://i.imgur.com/Zt6f5mB.gifv".into(),
            post_type: PostType::Video,
            crosspost_parent_list: None,
        };

        assert!(!db.existing_posts_for_subreddit(1, "absoluteunit").unwrap());
        db.record_post_seen_with_current_time(1, &post).unwrap();
        assert!(db.is_post_seen(1, &post).unwrap());
        assert!(db.existing_posts_for_subreddit(1, "absoluteunit").unwrap());
    }

    #[test]
    fn test_db_subscribe() {
        let config = Config::default();
        let mut db = Database::open(&config).unwrap();
        db.migrate().unwrap();
        let subscription_args = SubscriptionArgs {
            subreddit: "test".to_string(),
            limit: Some(1),
            time: Some(TopPostsTimePeriod::Week),
            filter: Some(PostType::Video),
        };
        db.subscribe(1, &subscription_args).unwrap();

        let subs = db.get_subscriptions_for_chat(1).unwrap();
        assert_eq!(
            subs,
            vec![Subscription {
                chat_id: 1,
                subreddit: "test".to_string(),
                limit: Some(1),
                time: Some(TopPostsTimePeriod::Week),
                filter: Some(PostType::Video),
            }]
        );
    }

    #[test]
    fn test_db_unsubscribe() {
        let config = Config::default();
        let mut db = Database::open(&config).unwrap();
        db.migrate().unwrap();
        let subscription_args = SubscriptionArgs {
            subreddit: "test".to_string(),
            limit: Some(1),
            time: Some(TopPostsTimePeriod::Week),
            filter: Some(PostType::Video),
        };
        db.subscribe(1, &subscription_args).unwrap();
        let subs = db.get_subscriptions_for_chat(1).unwrap();
        assert_eq!(subs.len(), 1);
        let deleted = db.unsubscribe(1, "test").unwrap();
        assert_eq!(deleted, "test");
        let subs = db.get_subscriptions_for_chat(1).unwrap();
        assert_eq!(subs, vec![]);
    }

    #[test]
    fn test_db_unsubscribe_deletes_posts() {
        let config = Config::default();
        let mut db = Database::open(&config).unwrap();
        db.migrate().unwrap();
        let subscription_args = SubscriptionArgs {
            subreddit: "test".to_string(),
            limit: Some(1),
            time: Some(TopPostsTimePeriod::Week),
            filter: Some(PostType::Video),
        };
        db.subscribe(1, &subscription_args).unwrap();
        let post = Post {
            id: "v6nu75".into(),
            created: 1654581100.0,
            post_hint: Some("link".into()),
            subreddit: "test".into(),
            title: "Tipping a cow to trim its hooves".into(),
            is_self: false,
            is_gallery: Some(false),
            is_video: false,
            gallery_data: None,
            media_metadata: None,
            ups: 469,
            permalink: "/r/test/comments/v6nu75/tipping_a_cow_to_trim_its_hooves/".into(),
            url: "https://i.imgur.com/Zt6f5mB.gifv".into(),
            post_type: PostType::Video,
            crosspost_parent_list: None,
        };
        db.record_post_seen_with_current_time(1, &post).unwrap();
        assert!(db.is_post_seen(1, &post).unwrap());
        db.unsubscribe(1, "test").unwrap();
        assert!(!db.is_post_seen(1, &post).unwrap());
    }
}
