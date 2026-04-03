#[cfg(any(feature = "mysql-zstd-compression", feature = "mysql-zlib-compression"))]
mod compression_tests {
    use sqlx::Row;
    use sqlx_mysql::MySql;
    use sqlx_test::new;

    #[sqlx_macros::test]
    async fn it_connects_with_compression() -> anyhow::Result<()> {
        let mut conn = new::<MySql>().await?;

        let rows = sqlx::raw_sql(r#"SHOW SESSION STATUS LIKE 'Compression'"#)
            .fetch_all(&mut conn)
            .await?;

        let result = rows
            .first()
            .map(|r| r.try_get::<String, _>(1).unwrap_or_default())
            .unwrap_or_default();

        assert!(!rows.is_empty());
        assert_eq!(result, "ON");

        Ok(())
    }
}
