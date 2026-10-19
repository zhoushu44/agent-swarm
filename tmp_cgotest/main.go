package main

import (
	"database/sql"
	"fmt"

	_ "github.com/mattn/go-sqlite3"
)

func main() {
	db, err := sql.Open("sqlite3", ":memory:")
	if err != nil {
		fmt.Println("open err:", err)
		return
	}
	defer db.Close()
	if _, err := db.Exec("create table t(a int)"); err != nil {
		fmt.Println("exec err:", err)
		return
	}
	fmt.Println("sqlite-ok")
}
