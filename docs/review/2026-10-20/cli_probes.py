# Diagnostic reproductions, not passing regression tests. All writes use new temporary directories.
import os, pathlib, subprocess, tempfile, json, signal
base=pathlib.Path(tempfile.mkdtemp(prefix='barqsync-cli-review-'))
repo=pathlib.Path(__file__).resolve().parents[3]
bin=str(repo/'target/debug/barqsync')
import shutil
ref=os.environ.get('RSYNC') or shutil.which('rsync')
if not ref:
 raise SystemExit('Install rsync 3.x or set RSYNC to its path before running these probes.')
if not subprocess.check_output([ref,'--version']).startswith(b'rsync  version 3'):
 raise SystemExit('The reference must be rsync 3.x; set RSYNC to its path.')
shim=base/'fake-ssh';shim.write_text('#!/bin/sh\nshift\nexec /bin/sh -c "$*"\n');shim.chmod(0o700)
def run(tool,cwd,args,timeout=7):
 p=subprocess.Popen([tool,*args],cwd=cwd,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
 try:
  out,err=p.communicate(timeout=timeout);return {'exit':p.returncode,'stdout':out.decode(errors='replace')[-400:],'stderr':err.decode(errors='replace')[-600:]}
 except subprocess.TimeoutExpired:
  os.killpg(p.pid,signal.SIGKILL);p.communicate();return {'timeout':timeout}
for case in ['multiple_roots_delete','delete_scan_error','backup_failed','same_file_inplace','same_file_remove']:
 for tool in [bin,ref]:
  d=base/(case+('-barq' if tool==bin else '-rsync'));d.mkdir()
  if case=='multiple_roots_delete':
   for n in ['a','b','dst']:(d/n).mkdir()
   (d/'a/alpha').write_text('alpha');(d/'b/beta').write_text('beta');(d/'dst/stale').write_text('stale')
   args=['-a','--delete','a/','b/','dst/']
  elif case=='delete_scan_error':
   (d/'src').mkdir();(d/'dst').mkdir();(d/'src/keep').symlink_to('missing-target');(d/'dst/keep').write_text('VALUABLE')
   args=['-aL','--delete','src/','dst/']
  elif case=='backup_failed':
   (d/'src').mkdir();(d/'dst').mkdir();(d/'src/f').write_text('NEW DATA');(d/'dst/f').write_text('OLD');(d/'dst/blocked').write_text('FILE')
   args=['-a','-b','--backup-dir=blocked/sub','src/','dst/']
  else:
   (d/'f').write_text('VALUABLE')
   args=['-a','--inplace','-I','f','f'] if case=='same_file_inplace' else ['-a','--remove-source-files','f','f']
  result=run(tool,d,args)
  result['files']={str(p.relative_to(d)):p.read_text() for p in d.rglob('*') if p.is_file() and not p.is_symlink()}
  print(json.dumps({'case':case,'tool':pathlib.Path(tool).name,**result}))
for n in [1,2]:
 d=base/f'threads{n}';d.mkdir();(d/'src').mkdir();(d/'src/f').write_text('TEST')
 r=run(bin,d,['-a',f'--threads={n}','-e',str(shim),'--remote-path',bin,'src/','fake:dst/'])
 print(json.dumps({'case':f'remote_threads_{n}',**r}))
print('fixtures='+str(base))
