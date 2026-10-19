import io
p = '_probe/probe.go'
s = io.open(p, encoding='utf-8').read()
old = 'total\\":3`), "pending after reap"'
new = 'total\\":2`), "pending after reap"'
assert old in s, 'pattern missing'
s = s.replace(old, new)
io.open(p, 'w', encoding='utf-8', newline='').write(s)
print('patched')
