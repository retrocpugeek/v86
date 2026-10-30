import sys
a=open(sys.argv[1]).read().split("\n"); b=open(sys.argv[2]).read().split("\n")
names=["pc"]+["r%d"%k for k in range(15)]
for i,(x,y) in enumerate(zip(a,b)):
    xr=x.split()[:16]; yr=y.split()[:16]
    if xr!=yr:
        print("line",i+1)
        for j in range(max(0,i-4),i+2):
            print(" ours", a[j]); print(" qemu", b[j])
        print("differs:", [(names[k],xr[k],yr[k]) for k in range(min(len(xr),len(yr))) if xr[k]!=yr[k]])
        break
else: print("no register difference in", min(len(a),len(b)))
