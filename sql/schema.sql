DROP TABLE IF EXISTS streams;
CREATE TABLE streams(
    id VARCHAR(10) NOT NULL,
    key_hash TEXT NOT NULL,
    PRIMARY KEY(id)
);