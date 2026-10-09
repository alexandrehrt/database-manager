-- Sample schema for manual checks. Portable: loads into both PostgreSQL and SQLite.
--   psql  -f fixtures/seed.sql
--   sqlite3 dbm.db < fixtures/seed.sql

DROP VIEW IF EXISTS order_totals;
DROP TABLE IF EXISTS shipments;
DROP TABLE IF EXISTS order_items;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS products;
DROP TABLE IF EXISTS customers;

CREATE TABLE customers (
    id         INTEGER PRIMARY KEY,
    name       VARCHAR(100) NOT NULL,
    email      VARCHAR(200) UNIQUE,
    vip        BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE products (
    sku   VARCHAR(20) PRIMARY KEY,
    title VARCHAR(200) NOT NULL,
    price NUMERIC(10, 2) NOT NULL
);

-- customer_id is nullable: guest orders have no customer and must show no FK link.
CREATE TABLE orders (
    id          INTEGER PRIMARY KEY,
    customer_id INTEGER REFERENCES customers (id),
    placed_at   TIMESTAMP NOT NULL,
    note        TEXT
);

CREATE INDEX orders_customer_idx ON orders (customer_id);

CREATE TABLE order_items (
    order_id INTEGER NOT NULL REFERENCES orders (id),
    line_no  INTEGER NOT NULL,
    sku      VARCHAR(20) NOT NULL REFERENCES products (sku),
    quantity INTEGER NOT NULL,
    PRIMARY KEY (order_id, line_no)
);

-- Composite foreign key onto order_items' two-column primary key.
CREATE TABLE shipments (
    id       INTEGER PRIMARY KEY,
    order_id INTEGER NOT NULL,
    line_no  INTEGER NOT NULL,
    carrier  VARCHAR(50) NOT NULL,
    CONSTRAINT shipments_item_fk FOREIGN KEY (order_id, line_no) REFERENCES order_items (order_id, line_no)
);

CREATE VIEW order_totals AS
SELECT o.id AS order_id, o.customer_id, SUM(i.quantity * p.price) AS total
FROM orders o
JOIN order_items i ON i.order_id = o.id
JOIN products p ON p.sku = i.sku
GROUP BY o.id, o.customer_id;

INSERT INTO customers (id, name, email, vip, created_at) VALUES
    (1, 'Ada Lovelace', 'ada@example.com', TRUE, '2024-01-05 10:00:00'),
    (2, 'Alan Turing', 'alan@example.com', FALSE, '2024-02-11 09:30:00'),
    (3, 'Grace Hopper', NULL, TRUE, '2024-03-20 14:15:00');

INSERT INTO products (sku, title, price) VALUES
    ('KB-01', 'Mechanical keyboard', 129.90),
    ('MS-02', 'Wireless mouse', 39.50),
    ('MN-27', '27" monitor; IPS', 349.00);

INSERT INTO orders (id, customer_id, placed_at, note) VALUES
    (100, 1, '2024-04-01 12:00:00', 'gift wrap'),
    (101, 2, '2024-04-02 08:45:00', NULL),
    (102, NULL, '2024-04-03 17:20:00', 'guest checkout'),
    (103, 1, '2024-04-05 11:10:00', 'it''s urgent; call first');

INSERT INTO order_items (order_id, line_no, sku, quantity) VALUES
    (100, 1, 'KB-01', 1),
    (100, 2, 'MS-02', 2),
    (101, 1, 'MN-27', 1),
    (102, 1, 'MS-02', 1),
    (103, 1, 'MN-27', 2);

INSERT INTO shipments (id, order_id, line_no, carrier) VALUES
    (1, 100, 1, 'DHL'),
    (2, 100, 2, 'DHL'),
    (3, 103, 1, 'FedEx');
