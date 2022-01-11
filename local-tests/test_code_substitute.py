#!/bin/env python
import json
import os
import os.path

import sys
from time import sleep

from chainrunner import *

SEND_RUNTIME = 'send-runtime/target/release/send_runtime'

CORRUPTED_BINARY = 'test-code-substitute/build/aleph-node'
FIXING_RUNTIME = 'test-code-substitute/build/fixing_runtime.wasm'
NEW_RUNTIME = 'test-code-substitute/build/new_runtime.wasm'

NODES = 4
WORKDIR = '.'

phrases = ['//Alice', '//Bob', '//Cedric', '//Dick']
keys = generate_keys(CORRUPTED_BINARY, phrases)


def query_runtime_version(nodes):
    print('Current version:')
    versions = set()
    for i, node in enumerate(nodes):
        sysver = node.rpc('system_version').result
        rt = node.rpc('state_getRuntimeVersion').result['specVersion']
        print(f'  Node {i}: system: {sysver}  runtime: {rt}')
        versions.add(rt)
    if len(versions) != 1:
        print(f'ERROR: nodes reported different runtime versions: {versions}')
    return max(versions)


def check_highest(nodes):
    results = [node.highest_block() for node in nodes]
    highest, finalized = zip(*results)
    print('Blocks seen by nodes:')
    print('  Highest:   ', *highest)
    print('  Finalized: ', *finalized)
    return max(finalized)


def check_build_files():
    assert os.path.isfile(CORRUPTED_BINARY)
    assert os.path.isfile(FIXING_RUNTIME)
    assert os.path.isfile(NEW_RUNTIME)


def run_corrupted_binary():
    print('Starting corrupted binary')
    chain = Chain(WORKDIR)
    chain.bootstrap(CORRUPTED_BINARY,
                    keys.values(),
                    sudo_account_id=keys[phrases[0]],
                    chain_type='local',
                    millisecs_per_block=2000,
                    session_period=40)

    chain.set_flags('validator',
                    port=Seq(30334),
                    ws_port=Seq(9944),
                    rpc_port=Seq(9933),
                    unit_creation_delay=200,
                    execution='Native')

    chain.set_log_level('afa', 'debug')
    chain.set_log_level('wasm_substitutes', 'debug')

    chain.start('corrupted')
    sleep(5)
    return chain


def panic(chain, message):
    print(f'💀 {message}')
    chain.stop()
    chain.purge()
    sys.exit(1)


def wait_for_stalling(chain):
    sleep(40)
    finalized_40 = check_highest(chain)
    print(f'There are {finalized_40} finalized blocks now. Waiting a little bit more.')

    sleep(10)
    finalized_50 = check_highest(chain)
    if finalized_50 != finalized_40:
        panic(chain, 'Chain is not running long enough to witness breakage.')
    print(f'There are still {finalized_50} finalized  blocks. Finalization stalled.')

    hash = chain[0].check_hash_of(finalized_50)
    if not hash:
        panic(chain, 'First node does not know hash of the highest finalized.')
    return hash, finalized_50


def update_chainspec(hash):
    print(f'Setting `code_substitute` with hash {hash}.')
    with open('chainspec.json', mode='r', encoding='utf-8') as chainspec_in:
        chainspec = json.loads(chainspec_in.read())
    with open(FIXING_RUNTIME, mode='rb') as fix:
        fix = fix.read().hex()

    chainspec['codeSubstitutes'] = {hash: fix}
    with open('chainspec-new.json', mode='w', encoding='utf-8') as chainspec_out:
        chainspec_out.write(json.dumps(chainspec))



def restart_nodes(chain):
    chain.stop()
    chain.set_chainspec('chainspec-new.json')
    chain.start('fixed')

    sleep(10)

    print('Chain restarted with a new chainspec')
    query_runtime_version(chain)


def wait_for_continuation(chain, stalled_at):
    sleep(10)
    finalized = check_highest(chain)
    if finalized == stalled_at:
        panic(chain, 'There are still troubles with finalization.')
    return finalized


def test_code_substitute():
    check_build_files()

    chain = run_corrupted_binary()
    query_runtime_version(chain)
    hash, finalized = wait_for_stalling(chain)

    update_chainspec(hash)
    restart_nodes(chain)

    wait_for_continuation(chain, finalized)


if __name__ == '__main__':
    test_code_substitute()
