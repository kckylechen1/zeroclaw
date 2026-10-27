#!/usr/bin/env python3
"""Device-maintenance client. Keys stay in 0600 files; no transcript logging.
Provision credentials separately over a verified, private operator channel.
"""
import argparse
import contextlib
import fcntl
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import secrets
import socket
import stat
import time

class ProtocolError(Exception):
    pass

def valid_id(s):
    return isinstance(s,str) and re.fullmatch(r'[a-z0-9_-]{1,32}',s) is not None

def ishex(s):
    return isinstance(s,str) and re.fullmatch(r'[0-9a-f]{64}',s) is not None

def mac(key,*fields):
    return hmac.new(key,b''.join(s.encode('ascii')+b'\0' for s in fields),hashlib.sha256).hexdigest()

def validate(data):
    if not isinstance(data,dict) or set(data)!={'version','host','key','last_seq'}:
        raise ProtocolError('credential_format')
    if type(data['version']) is not int or data['version']!=1 or not valid_id(data['host']) or not ishex(data['key']):
        raise ProtocolError('credential_format')
    if type(data['last_seq']) is not int or not 0<=data['last_seq']<2**64:
        raise ProtocolError('credential_seq')
    return data

def private_dir(path):
    path=Path(path)
    if not path.exists():path.mkdir(mode=0o700)
    m=path.lstat()
    if not stat.S_ISDIR(m.st_mode) or m.st_uid!=os.getuid() or stat.S_IMODE(m.st_mode)!=0o700:
        raise ProtocolError('directory_permissions')

def read_credentials(path):
    path=Path(path);m=path.lstat()
    if not stat.S_ISREG(m.st_mode) or m.st_uid!=os.getuid() or stat.S_IMODE(m.st_mode)!=0o600 or m.st_size>2048:
        raise ProtocolError('credential_permissions')
    with path.open('rb') as f:
        opened=os.fstat(f.fileno())
        if (opened.st_dev,opened.st_ino)!=(m.st_dev,m.st_ino):raise ProtocolError('credential_changed')
        data=f.read(2049)
    if len(data)>2048:raise ProtocolError('credential_size')
    return validate(json.loads(data))

def save_credentials(path,data):
    validate(data);path=Path(path);private_dir(path.parent)
    tmp=path.parent/('.credential-'+secrets.token_hex(8))
    fd=os.open(tmp,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
    try:
        with os.fdopen(fd,'wb') as f:
            f.write((json.dumps(data,separators=(',',':'))+'\n').encode());f.flush();os.fsync(f.fileno())
        os.replace(tmp,path)
        fd=os.open(path.parent,os.O_RDONLY)
        try:os.fsync(fd)
        finally:os.close(fd)
    finally:
        if tmp.exists():tmp.unlink()

@contextlib.contextmanager
def credential_lock(path):
    path=Path(path);private_dir(path.parent)
    flags=os.O_RDWR|os.O_CREAT|os.O_NOFOLLOW
    fd=os.open(str(path)+'.lock',flags,0o600)
    try:
        m=os.fstat(fd)
        if not stat.S_ISREG(m.st_mode) or m.st_uid!=os.getuid() or stat.S_IMODE(m.st_mode)!=0o600:
            raise ProtocolError('lock_permissions')
        fcntl.flock(fd,fcntl.LOCK_EX)
        yield
    finally:os.close(fd)

def readline(sock,seconds=4):
    end=time.monotonic()+seconds;b=bytearray()
    while len(b)<512:
        left=end-time.monotonic()
        if left<=0:raise ProtocolError('deadline')
        sock.settimeout(left);x=sock.recv(1)
        if not x:raise ProtocolError('closed')
        if x==b'\n':
            s=b.decode('ascii');parts=s.split(' ')
            if any(not p for p in parts):raise ProtocolError('frame')
            return parts
        b+=x
    raise ProtocolError('frame_size')

class Session:
    def __init__(self,address,credential,path=None):
        self.credential=validate(dict(credential));self.path=path
        self.key=bytes.fromhex(self.credential['key']);self.host=self.credential['host']
        self.sock=socket.create_connection(address,timeout=4)
        try:
            hello=readline(self.sock)
            if len(hello)!=4 or hello[0]!='N3B1' or not all(ishex(x) for x in hello[1:]):raise ProtocolError('hello')
            _,self.epoch,self.instance,self.server=hello
            self.client=secrets.token_hex(32)
            self.auth_frame=self.authentication_frame()
            self.sock.sendall(self.auth_frame)
            ready=readline(self.sock)
            if len(ready)!=4 or ready[0]!='READY' or not ready[1].isdigit() or str(int(ready[1]))!=ready[1] or not ishex(ready[2]) or not ishex(ready[3]):
                raise ProtocolError('authentication')
            self.floor=int(ready[1]);self.turn=ready[2]
            if self.floor>=2**64:raise ProtocolError('server_seq')
            expected=mac(self.key,'node3b/ready/v1',self.epoch,self.instance,self.server,self.host,self.client,ready[1],self.turn)
            if not hmac.compare_digest(expected,ready[3]):raise ProtocolError('server_proof')
        except BaseException:
            self.sock.close();raise

    def authentication_frame(self):
        proof=mac(self.key,'node3b/auth/v1',self.epoch,self.instance,self.server,self.host,self.client)
        return f'AUTH {self.host} {self.client} {proof}\n'.encode()

    def frame(self,seq,verb,arg):
        if not isinstance(seq,int) or not 0<seq<2**64:raise ProtocolError('seq')
        if not re.fullmatch(r'[a-z]+',verb) or not re.fullmatch(r'[A-Za-z0-9_-]{1,64}',arg):raise ProtocolError('command')
        proof=mac(self.key,'node3b/cmd/v1',self.epoch,self.instance,self.server,self.host,self.client,self.turn,str(seq),verb,arg)
        return f'CMD {seq} {verb} {arg} {proof}\n'.encode()

    def command(self,verb,arg='-'):
        seq=max(self.floor,self.credential['last_seq'])+1
        frame=self.frame(seq,verb,arg)
        self.credential['last_seq']=seq
        if self.path is not None:save_credentials(self.path,self.credential)
        self.sock.sendall(frame)
        r=readline(self.sock)
        if len(r)!=6 or r[0]!='RESULT' or r[1]!=str(seq) or r[2] not in ('OK','ERR') or not ishex(r[4]) or not ishex(r[5]):
            raise ProtocolError('response')
        expected=mac(self.key,'node3b/result/v1',self.epoch,self.instance,self.server,self.host,self.client,self.turn,r[1],r[2],r[3],r[4])
        if not hmac.compare_digest(expected,r[5]):raise ProtocolError('response_proof')
        self.floor=seq;self.turn=r[4]
        return {'seq':seq,'status':r[2],'result':r[3]}

    def close(self):self.sock.close()
    def __enter__(self):return self
    def __exit__(self,*args):self.close()

def main():
    os.umask(0o077)
    parser=argparse.ArgumentParser(description='Authenticated device-maintenance client')
    parser.add_argument('--address',required=True)
    parser.add_argument('--port',type=int,required=True)
    parser.add_argument('--credentials',required=True)
    parser.add_argument('verb',choices=['info','capabilities','face'])
    parser.add_argument('argument',nargs='?',default='-')
    args=parser.parse_args()
    try:
        with credential_lock(args.credentials):
            with Session((args.address,args.port),read_credentials(args.credentials),args.credentials) as session:
                print(json.dumps(session.command(args.verb,args.argument)))
    except Exception:
        print('ERR operation_failed',file=__import__('sys').stderr)
        return 1
    return 0

if __name__=='__main__':raise SystemExit(main())
