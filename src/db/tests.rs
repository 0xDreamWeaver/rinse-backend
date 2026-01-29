#[cfg(test)]
mod tests {
    use super::super::Database;
    use anyhow::Result;

    async fn setup_test_db() -> Result<Database> {
        let db = Database::new("sqlite::memory:").await?;
        Ok(db)
    }

    #[tokio::test]
    async fn test_create_and_get_user() -> Result<()> {
        let db = setup_test_db().await?;

        let user = db.create_user("testuser", "hashed_password").await?;
        assert_eq!(user.username, "testuser");
        assert_eq!(user.password_hash, "hashed_password");

        let fetched_user = db.get_user_by_username("testuser").await?;
        assert!(fetched_user.is_some());
        assert_eq!(fetched_user.unwrap().username, "testuser");

        Ok(())
    }

    #[tokio::test]
    async fn test_create_duplicate_user_fails() -> Result<()> {
        let db = setup_test_db().await?;

        db.create_user("testuser", "hash1").await?;
        let result = db.create_user("testuser", "hash2").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_create_and_get_item() -> Result<()> {
        let db = setup_test_db().await?;

        let item = db.create_item(
            "test.mp3",
            "test query",
            "/tmp/test.mp3",
            1024000,
            Some(320),
            Some(180),
            "mp3",
            "source_user",
        ).await?;

        assert_eq!(item.filename, "test.mp3");
        assert_eq!(item.bitrate, Some(320));

        let fetched_item = db.get_item(item.id).await?;
        assert!(fetched_item.is_some());
        assert_eq!(fetched_item.unwrap().filename, "test.mp3");

        Ok(())
    }

    #[tokio::test]
    async fn test_get_all_items() -> Result<()> {
        let db = setup_test_db().await?;

        db.create_item("item1.mp3", "query1", "/tmp/1.mp3", 1000, None, None, "mp3", "user1").await?;
        db.create_item("item2.mp3", "query2", "/tmp/2.mp3", 2000, None, None, "mp3", "user2").await?;

        let items = db.get_all_items().await?;
        assert_eq!(items.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_item_status() -> Result<()> {
        let db = setup_test_db().await?;

        let item = db.create_item(
            "test.mp3",
            "query",
            "/tmp/test.mp3",
            1000,
            None,
            None,
            "mp3",
            "user",
        ).await?;

        db.update_item_status(item.id, "completed", 1.0, None).await?;

        let updated_item = db.get_item(item.id).await?.unwrap();
        assert_eq!(updated_item.download_status, "completed");
        assert_eq!(updated_item.download_progress, 1.0);

        Ok(())
    }

    #[tokio::test]
    async fn test_create_and_get_list() -> Result<()> {
        let db = setup_test_db().await?;
        let user = db.create_user("testuser", "hash").await?;

        let list = db.create_list("My Playlist", user.id, 5).await?;
        assert_eq!(list.name, "My Playlist");
        assert_eq!(list.total_items, 5);

        let fetched_list = db.get_list(list.id).await?;
        assert!(fetched_list.is_some());
        assert_eq!(fetched_list.unwrap().name, "My Playlist");

        Ok(())
    }

    #[tokio::test]
    async fn test_add_item_to_list() -> Result<()> {
        let db = setup_test_db().await?;
        let user = db.create_user("testuser", "hash").await?;

        let list = db.create_list("My List", user.id, 2).await?;
        let item1 = db.create_item("song1.mp3", "q1", "/tmp/1.mp3", 1000, None, None, "mp3", "u1").await?;
        let item2 = db.create_item("song2.mp3", "q2", "/tmp/2.mp3", 2000, None, None, "mp3", "u2").await?;

        db.add_item_to_list(list.id, item1.id, 0).await?;
        db.add_item_to_list(list.id, item2.id, 1).await?;

        let items = db.get_list_items(list.id).await?;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].filename, "song1.mp3");
        assert_eq!(items[1].filename, "song2.mp3");

        Ok(())
    }

    #[tokio::test]
    async fn test_batch_delete_items() -> Result<()> {
        let db = setup_test_db().await?;

        let item1 = db.create_item("1.mp3", "q", "/tmp/1.mp3", 1000, None, None, "mp3", "u").await?;
        let item2 = db.create_item("2.mp3", "q", "/tmp/2.mp3", 1000, None, None, "mp3", "u").await?;
        let item3 = db.create_item("3.mp3", "q", "/tmp/3.mp3", 1000, None, None, "mp3", "u").await?;

        db.delete_items(&[item1.id, item2.id]).await?;

        let items = db.get_all_items().await?;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, item3.id);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_user_lists() -> Result<()> {
        let db = setup_test_db().await?;
        let user1 = db.create_user("user1", "hash1").await?;
        let user2 = db.create_user("user2", "hash2").await?;

        db.create_list("List 1", user1.id, 1).await?;
        db.create_list("List 2", user1.id, 2).await?;
        db.create_list("List 3", user2.id, 3).await?;

        let user1_lists = db.get_user_lists(user1.id).await?;
        assert_eq!(user1_lists.len(), 2);

        let user2_lists = db.get_user_lists(user2.id).await?;
        assert_eq!(user2_lists.len(), 1);

        Ok(())
    }
}
