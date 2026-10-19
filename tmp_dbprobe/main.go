package main

import (
	"database/sql"
	"fmt"
	"os"

	_ "modernc.org/sqlite"
)

func main() {
	db, err := sql.Open("sqlite", os.Args[1])
	if err != nil {
		panic(err)
	}
	defer db.Close()
	rows, err := db.Query("select type,name,sql from sqlite_master where sql is not null order by type,name")
	if err != nil {
		panic(err)
	}
	defer rows.Close()
	for rows.Next() {
		var t, n, s string
		if err := rows.Scan(&t, &n, &s); err != nil {
			panic(err)
		}
		fmt.Printf("--- %s %s\n%s\n", t, n, s)
	}
}
