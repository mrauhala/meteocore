"""Regenerate pvol-cold-batch.h5 with h5py and numpy (test dependencies only)."""
from pathlib import Path

import h5py
import numpy as np
with h5py.File(Path(__file__).with_name('pvol-cold-batch.h5'), 'w', libver='earliest') as f:
    g=f.create_group('what')
    for k,v in {'object':'PVOL','date':'20260916','time':'120000','source':'NOD:test'}.items(): g.attrs[k]=np.bytes_(v)
    g=f.create_group('where')
    for k,v in {'lon':24.5,'lat':60.3,'height':100.0}.items(): g.attrs[k]=v
    for n in (1,2):
        d=f.create_group(f'dataset{n}'); w=d.create_group('where')
        for k,v in {'elangle':n*0.5,'nbins':8,'nrays':4,'rscale':1000.0,'rstart':0.0,'a1gate':0}.items(): w.attrs[k]=v
        for m,q in enumerate(('DBZH','VRAD'),1):
            g=d.create_group(f'data{m}'); w=g.create_group('what')
            w.attrs['quantity']=np.bytes_(q)
            for k,v in {'gain':0.5,'offset':-32.0,'nodata':255.0,'undetect':0.0}.items(): w.attrs[k]=v
            g.create_dataset('data',data=np.full((4,8),n*10+m,dtype='u1'))
