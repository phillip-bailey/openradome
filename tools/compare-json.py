import json, sys
from collections import Counter
d=sys.argv[1]
up=json.load(open(d+'/up.json')); rs=json.load(open(d+'/rs.json'))
print("snapshot delta %.1f s | messages upstream %d rust %d"%(rs['now']-up['now'],up['messages'],rs['messages']))
u={a['hex']:a for a in up['aircraft']}; r={a['hex']:a for a in rs['aircraft']}
print("aircraft: upstream",len(u),"rust",len(r),"| only upstream",sorted(set(u)-set(r)),"| only rust",sorted(set(r)-set(u)))
skip={'seen','seen_pos','rssi','messages','r_dst','r_dir'}
diff=Counter(); ex={}; n=0
for h in sorted(set(u)&set(r)):
    for k in set(u[h])|set(r[h]):
        if k in skip: continue
        n+=1
        a,b=u[h].get(k),r[h].get(k)
        if a!=b:
            if isinstance(a,(int,float)) and isinstance(b,(int,float)) and abs(a-b)<(1e-4 if k in('lat','lon') else 0.051): continue
            diff[k]+=1; ex.setdefault(k,[]).append((h,a,b))
print("fields compared:",n,"| differing:",sum(diff.values()))
for k,c in diff.most_common(): print(f"  {k:18s} {c:3d}  e.g. {ex[k][:3]}")
