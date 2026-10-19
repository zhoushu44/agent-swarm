import io
p = '_probe/probe.go'
s = io.open(p, encoding='utf-8').read()
q = chr(34)
old = 'dsn := ' + q + 'file:probe2?mode=memory&cache=shared' + q
new = 'dsn := ' + q + 'probe2.db' + q
if old not in s:
    print('missing dsn')
else:
    s = s.replace(old, new)
    s = s.replace('fmt.Println(' + q + 'ALL PROBE CHECKS PASSED' + q + ')', 'os.Remove(' + q + 'probe2.db' + q + ')\n\tfmt.Println(' + q + 'ALL PROBE CHECKS PASSED' + q + ')')
    s = s.replace('import (' + chr(10) + chr(9) + q + 'fmt' + q, 'import (' + chr(10) + chr(9) + q + 'fmt' + q + chr(10) + chr(9) + q + 'os' + q)
    io.open(p, 'w', encoding='utf-8', newline='').write(s)
    print('patched')
