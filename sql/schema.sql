DROP TABLE IF EXISTS streams;
CREATE TABLE streams(
    id VARCHAR(20) NOT NULL,
    key_hash TEXT NOT NULL,
    PRIMARY KEY(id)
);