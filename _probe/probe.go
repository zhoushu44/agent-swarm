package main

import (
	"fmt"
	"net/http/httptest"
	"strings"

	"swarm-go/internal/server"
	"swarm-go/internal/store"
)

func check(cond bool, msg string) {
	if !cond {
		panic(msg)
	}
}

func main() {
	clock := int64(1000)
	tick := func() int64 { clock += 10; return clock }

	st, err := store.Open("file:probe1?mode=memory&cache=shared")
	if err != nil {
		panic(err)
	}
	defer st.Close()
	st.SetClock(tick)

	srv := server.New(st)
	srv.SetClock(tick)
	do := func(method, target, body string) *httptest.ResponseRecorder {
		var r = httptest.NewRequest(method, target, strings.NewReader(body))
		if body == "" {
			r = httptest.NewRequest(method, target, nil)
		}
		w := httptest.NewRecorder()
		srv.ServeHTTP(w, r)
		return w
	}

	// create items
	w := do("POST", "/item", `{"peer":"w1","title":"a"}`)
	check(w.Code == 200, "create a: "+w.Body.String())
	w = do("POST", "/item", `{"peer":"w1","title":"b","priority":5}`)
	check(w.Code == 200, "create b: "+w.Body.String())
	w = do("POST", "/item", `{"peer":"w1","title":"child","parentId":"t1"}`)
	check(w.Code == 200, "create child: "+w.Body.String())
	w = do("POST", "/item", `{"peer":"w1","title":"bad","parentId":"nope"}`)
	check(w.Code == 404, fmt.Sprintf("bad parent code=%d body=%s", w.Code, w.Body.String()))

	// board: priority desc first
	w = do("GET", "/board", "")
	fmt.Println("board:", w.Body.String())
	check(strings.Contains(w.Body.String(), `"total":3`), "total 3")

	// claim
	w = do("POST", "/claim", `{"itemId":"t1","peer":"w1"}`)
	check(w.Code == 200, "claim: "+w.Body.String())
	fmt.Println("claim:", w.Body.String())
	claimBody := w.Body.String()
	idx := strings.Index(claimBody, `"token":"`)
	tok := claimBody[idx+len(`"token":"`):]
	tok = tok[:strings.Index(tok, `"`)]

	// claim again -> 409
	w = do("POST", "/claim", `{"itemId":"t1","peer":"w2"}`)
	check(w.Code == 409, fmt.Sprintf("reclaim code=%d", w.Code))

	// unknown item -> 404
	w = do("POST", "/claim", `{"itemId":"zzz","peer":"w2"}`)
	check(w.Code == 404, fmt.Sprintf("claim zzz code=%d", w.Code))

	// context attached to the item, then history via claim of another item
	w = do("POST", "/context", `{"peer":"w1","kind":"FAIL","text":"tried X, broke","itemId":"t1","tags":["a","b"]}`)
	check(w.Code == 200, "context: "+w.Body.String())
	w = do("GET", "/item?id=t1", "")
	check(strings.Contains(w.Body.String(), "tried X"), "history has entry: "+w.Body.String())
	check(strings.Contains(w.Body.String(), `"tags":["a","b"]`), "tags roundtrip: "+w.Body.String())

	// search
	w = do("GET", "/context/search?q=broke", "")
	check(strings.Contains(w.Body.String(), `"count":1`), "search: "+w.Body.String())
	w = do("GET", "/context/search?q=nothing", "")
	check(strings.Contains(w.Body.String(), `"count":0`), "search none: "+w.Body.String())

	// renew
	w = do("POST", "/renew", `{"token":"`+tok+`"}`)
	check(w.Code == 200, "renew: "+w.Body.String())
	w = do("POST", "/renew", `{"token":"nope"}`)
	check(w.Code == 404, fmt.Sprintf("renew nope code=%d", w.Code))

	// digest
	w = do("GET", "/digest?peer=w1&since=0", "")
	fmt.Println("digest:", w.Body.String()[:400])
	check(strings.Contains(w.Body.String(), `"inProgress":1`), "digest inProgress")
	check(strings.Contains(w.Body.String(), tok), "digest has claim token")

	// submit
	w = do("POST", "/submit", `{"token":"`+tok+`","peer":"w1","commitSha":"abc123"}`)
	check(w.Code == 200, "submit: "+w.Body.String())
	check(strings.Contains(w.Body.String(), `"status":"submitted"`), "submitted status: "+w.Body.String())
	w = do("POST", "/submit", `{"token":"`+tok+`","peer":"w1","commitSha":"abc123"}`)
	check(w.Code == 404, fmt.Sprintf("resubmit code=%d", w.Code))
	w = do("POST", "/submit", `{"token":"x","peer":"w1","commitSha":""}`)
	check(w.Code == 400, fmt.Sprintf("submit nosha code=%d", w.Code))

	// release flow
	w = do("POST", "/claim", `{"itemId":"t2","peer":"w2"}`)
	check(w.Code == 200, "claim t2: "+w.Body.String())
	b := w.Body.String()
	i := strings.Index(b, `"token":"`)
	t2 := b[i+len(`"token":"`):]
	t2 = t2[:strings.Index(t2, `"`)]
	w = do("POST", "/release", `{"token":"`+t2+`"}`)
	check(w.Code == 200, "release: "+w.Body.String())
	w = do("GET", "/board?status=pending", "")
	check(strings.Contains(w.Body.String(), `"total":2`), "pending after release: "+w.Body.String())
	w = do("POST", "/release", `{"token":"`+t2+`"}`)
	check(w.Code == 404, fmt.Sprintf("release twice code=%d", w.Code))

	// messages
	w = do("POST", "/message", `{"from":"w1","to":"w2","text":"hi"}`)
	check(w.Code == 200, "message: "+w.Body.String())
	w = do("GET", "/messages?peer=w2&since=0", "")
	check(strings.Contains(w.Body.String(), `"count":1`), "messages: "+w.Body.String())
	w = do("GET", "/messages?peer=w3&since=0", "")
	check(strings.Contains(w.Body.String(), `"count":0`), "messages w3: "+w.Body.String())

	// reap: expire the claim on t1 (released) -> claim t3 with short lease
	st.SetLease(5)
	w = do("POST", "/claim", `{"itemId":"t3","peer":"w3"}`)
	check(w.Code == 200, "claim t3: "+w.Body.String())
	clock += 100
	n, err := st.ReapExpired()
	check(err == nil, fmt.Sprint(err))
	check(n == 1, fmt.Sprintf("reaped=%d", n))
	w = do("GET", "/board?status=pending", "")
	check(strings.Contains(w.Body.String(), `"total":2`), "pending after reap: "+w.Body.String())

	// persistence across reopen
	dir := "file:probe2?mode=memory&cache=shared"
	st2, err := store.Open(dir)
	check(err == nil, fmt.Sprint(err))
	if _, err := st2.CreateItem(server.CreateItemInput{Peer: "w1", Title: "keep"}); err != nil {
		panic(err)
	}
	st2.Close()
	st3, err := store.Open(dir)
	check(err == nil, fmt.Sprint(err))
	items, total, err := st3.ListItems("", 10, 0)
	check(err == nil, fmt.Sprint(err))
	check(total == 1 && items[0].Title == "keep", fmt.Sprintf("reopen total=%d items=%v", total, items))
	st3.Close()

	fmt.Println("ALL PROBE CHECKS PASSED")
}
