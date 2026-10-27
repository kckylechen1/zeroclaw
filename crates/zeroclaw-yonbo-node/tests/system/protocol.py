#!/usr/bin/env python3
"""Native loopback protocol matrix. Generated fixture secrets are never logged."""
from pathlib import Path
import json, os, socket, subprocess, tempfile, time, sys
sys.path.insert(0,str(Path(__file__).resolve().parents[2]/"tools"))
from client import Session, ProtocolError, readline, mac, validate, save_credentials

ROOT=Path(__file__).resolve().parents[2]
BIN=Path(sys.argv[1]).resolve()
results=[]
def passed(name):results.append(name);print('PASS',name,flush=True)
def closed(s):
    s.settimeout(2)
    try:assert s.recv(1024)==b''
    except (ConnectionResetError,BrokenPipeError):pass
def rejected(fn):
    try:fn()
    except (ProtocolError,OSError,ValueError):return
    raise AssertionError('unexpected_acceptance')

def main():
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix='yonbo-protocol-') as td:
        data=Path(td)/'node';data.mkdir(mode=0o700)
        with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
        address=('127.0.0.1',port)
        base=[str(BIN),'--data-dir',str(data),'--listen',f'127.0.0.1:{port}']
        def manage(mode):
            p=subprocess.run(base[:3]+[mode,'fixture'],capture_output=True,timeout=8)
            assert p.returncode==0, 'management failed (secret output suppressed): '+p.stderr.decode(errors='replace')
            if mode=='--pair':return validate(json.loads(p.stdout))
            assert p.stdout==b'REVOKED\n'
        def floor():return json.loads((data/'keys.json').read_text())['hosts'][0]['last_seq']
        def start(extra=()):
            p=subprocess.Popen(base+list(extra)+['--serve'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            end=time.monotonic()+3
            while time.monotonic()<end:
                if p.poll() is not None:raise AssertionError('server failed')
                try:
                    r=json.loads((data/'runtime.json').read_text())
                    if r['pid']==p.pid:return p,r
                except (FileNotFoundError,ValueError):pass
                time.sleep(.02)
            p.kill();p.wait();raise AssertionError('startup timeout')
        credential=manage('--pair')
        assert (data/'keys.json').stat().st_mode&0o777==0o600
        passed('pair_32byte_private_store')
        server,runtime=start()
        try:
            with Session(address,credential) as s:
                assert s.command('info')['status']=='OK'
                assert s.command('capabilities')['result']=='info,capabilities'
                assert s.command('face','KaiXin')['result']=='face_disabled'
                auth=s.auth_frame
            first=floor();passed('correct_key_and_signed_responses')
            host=Path(td)/'host';host.mkdir(mode=0o700);credential_path=host/'credential.json'
            save_credentials(credential_path,credential)
            cli=subprocess.run([sys.executable,'-B',str(ROOT/'tools/client.py'),'--address',address[0],'--port',str(port),'--credentials',str(credential_path),'info'],capture_output=True,timeout=6)
            assert cli.returncode==0 and json.loads(cli.stdout)['status']=='OK'
            assert credential['key'].encode() not in cli.stdout+cli.stderr
            first=floor();passed('documented_client_cli_authenticated_info')
            bad=dict(credential);bad['key']='00'*32
            rejected(lambda:Session(address,bad));assert floor()==first;passed('wrong_key_no_sequence_change')
            with socket.create_connection(address) as s:
                readline(s);s.sendall(auth);closed(s)
            passed('old_handshake_rejected')
            with socket.create_connection(address) as s:
                _,ep,instance,nonce=readline(s);client='ab'*32
                proof=mac(bytes.fromhex(credential['key']),'node3b/auth/v1','00'*32,instance,nonce,'fixture',client)
                s.sendall(f'AUTH fixture {client} {proof}\n'.encode());closed(s)
            passed('forged_epoch_rejected')
            for frame in [b'face KaiXin\n',b'CMD 1 face KaiXin '+b'0'*64+b'\n',b'x'*513,b'AUTH\x00\n']:
                with socket.create_connection(address) as s:readline(s);s.sendall(frame);closed(s)
            assert floor()==first;passed('unauth_face_and_malformed_frames_rejected')
            with Session(address,credential) as s:
                before=floor();frame=s.frame(before+1,'info','-')
                corrupted=frame[:-2]+(b'1' if frame[-2:-1]!=b'1' else b'2')+b'\n'
                s.sock.sendall(corrupted);closed(s.sock);assert floor()==before
            passed('tampered_command_mac_rejected')
            with Session(address,credential) as s:
                result=s.command('info');s.sock.sendall(s.frame(result['seq'],'info','-'));closed(s.sock)
            passed('same_seq_new_turn_rejected')
            with Session(address,credential) as s:
                s.sock.sendall(s.frame(max(1,s.floor-1),'info','-'));closed(s.sock)
            passed('decreasing_seq_rejected')
            with Session(address,credential) as s:
                frame=s.frame(s.floor+1,'info','-')
                result=s.command('info')
                s.sock.sendall(frame);closed(s.sock)
            passed('old_command_turn_rejected')
            with Session(address,credential) as s:assert s.command('info')['seq']>result['seq']
            passed('disconnect_reconnect_seq_continues')
            a=Session(address,credential);b=Session(address,credential)
            manage('--revoke');closed(a.sock);closed(b.sock);a.close();b.close()
            assert json.loads((data/'keys.json').read_text())['hosts'][0]['revoked'] is True
            rejected(lambda:Session(address,credential));passed('revoke_closes_existing_and_denies_new')
            old=credential;credential=manage('--pair');assert old['key']!=credential['key']
            rejected(lambda:Session(address,old))
            with Session(address,credential) as s:assert s.command('info')['status']=='OK'
            passed('re_pair_rotates_key_and_restores_access')
            with Session(address,credential) as s:
                assert s.command('pair','fixture')['status']=='ERR'
                assert s.command('revoke','fixture')['status']=='ERR'
            passed('management_unavailable_over_network')
            extra=subprocess.run(base+['--serve'],capture_output=True,timeout=2)
            assert extra.returncode!=0
            assert json.loads((data/'runtime.json').read_text())['pid']==server.pid
            passed('single_daemon_lock')
            with Session(address,credential) as s:
                time.sleep(10.2);closed(s.sock)
            passed('turn_expiry_closes_idle_session')
            pending=[]
            try:
                for _ in range(4):
                    s=socket.create_connection(address);readline(s);pending.append(s)
                with socket.create_connection(address) as extra:closed(extra)
            finally:
                for s in pending:s.close()
            passed('four_connection_limit')
            time.sleep(.1)
            s=Session(address,credential);last=s.command('info')['seq'];old_auth=s.auth_frame
            server.kill();server.wait();closed(s.sock);s.close()
            server,new_runtime=start()
            assert runtime['epoch']==new_runtime['epoch'] and runtime['instance']!=new_runtime['instance']
            with socket.create_connection(address) as s:readline(s);s.sendall(old_auth);closed(s)
            with Session(address,credential) as s:assert s.command('info')['seq']>last
            passed('kill_restart_persistent_seq_new_instance_old_session_rejected')
            def stop_frame(frame):
                with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as u:
                    u.settimeout(2);u.connect(str(data/'admin.sock'));u.sendall(frame);return u.recv(256)
            assert stop_frame(f"STOP {new_runtime['epoch']} {'0'*64} {server.pid}\n".encode()).startswith(b'ERR ')
            assert server.poll() is None
            assert stop_frame(f"STOP {new_runtime['epoch']} {new_runtime['instance']} {server.pid}\n".encode())==b'STOPPING\n'
            server.wait(timeout=3);assert server.returncode==0
            assert not (data/'runtime.json').exists()
            passed('local_stop_bound_to_instance_without_pid_signals')
            # Repository-relevant owner fixtures cross the real process boundary.
            enabled=Path(td)/'ENABLED';cancel=Path(td)/'CANCEL'
            options=['--owner-enabled',str(enabled),'--owner-cancel',str(cancel)]
            p=subprocess.run(base+options+['--serve'],capture_output=True,timeout=3)
            assert p.returncode!=0 and not (data/'runtime.json').exists()
            enabled.touch();cancel.touch()
            p=subprocess.run(base+options+['--serve'],capture_output=True,timeout=3)
            assert p.returncode!=0 and not (data/'runtime.json').exists()
            cancel.unlink();server,managed=start(options)
            enabled.unlink();server.wait(timeout=3);assert server.returncode==0
            assert not (data/'runtime.json').exists()
            enabled.touch();server,managed=start(options)
            cancel.touch();server.wait(timeout=3);assert server.returncode==0
            assert not (data/'runtime.json').exists()
            passed('owner_missing_enable_cancel_latch_and_live_marker_exit')
            # Explicit local display opt-in, fixed D-Bus signature/allowlist.
            calls=Path(td)/'calls';sender=Path(td)/'sender'
            sender.write_text('#!/bin/sh\nprintf "%s\\n" "$@" >> '+str(calls)+'\n')
            sender.chmod(0o700)
            server,face_runtime=start(['--dbus-send',str(sender),'--dbus-address','unix:path='+str(Path(td)/'bus')])
            with socket.create_connection(address) as s:
                readline(s);s.sendall(b'face KaiXin\n');closed(s)
            assert not calls.exists()
            with Session(address,credential) as s:
                assert s.command('capabilities')['result']=='face,info,capabilities'
                assert s.command('face','NotAllowed')['status']=='ERR'
                assert not calls.exists()
                assert s.command('face','KaiXin')['result']=='sent:Happy'
            assert calls.read_text().splitlines()==['--bus=unix:path='+str(Path(td)/'bus'),'--type=signal','/xapp/xuanyuan/interface','com.xuanyuan.interface.EmojiPlaySignal','string:Happy','int32:1']
            server.terminate();server.wait(timeout=3)
            passed('display_requires_opt_in_auth_and_allowlist_fixed_signature')
            original=(data/'keys.json').read_bytes();(data/'keys.json').write_bytes(b'{invalid')
            p=subprocess.run(base+['--serve'],capture_output=True,timeout=3);assert p.returncode!=0
            (data/'keys.json').write_bytes(original);(data/'keys.json').chmod(0o644)
            p=subprocess.run(base+['--serve'],capture_output=True,timeout=3);assert p.returncode!=0
            passed('corrupt_or_world_readable_store_fails_closed')
        finally:
            if server.poll() is None:server.terminate();server.wait(timeout=3)
    print('PROTOCOL_CASES_PASSED',len(results),flush=True)

if __name__=='__main__':main()
