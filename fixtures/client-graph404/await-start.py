import os,pathlib,subprocess,sys,time
barrier=pathlib.Path('/stage/evidence/barriers')/(sys.argv[1]+'.ready')
start=time.monotonic()
while not barrier.exists():
    if time.monotonic()-start>30:raise RuntimeError('Namespace setup gate deadline')
    time.sleep(.05)
os.execvp(sys.argv[2],sys.argv[2:])
