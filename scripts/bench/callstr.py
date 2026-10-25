def work(x):
    return f"  {x}  ".strip()


last = ""
i = 0
while i < 20000:
    last = work(i)
    i = i + 1
print(last)
