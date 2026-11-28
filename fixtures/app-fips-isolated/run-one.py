import json,pathlib
v=json.loads(pathlib.Path('/stage/evidence/lock/resolution.json').read_text()); assert v['passed']; print('Lock preparation complete; full product qualification remains required.')
