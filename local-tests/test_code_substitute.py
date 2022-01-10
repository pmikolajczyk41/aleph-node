#!/bin/env python
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

    chain.start('corrupted')
    sleep(5)
    return chain


def panic(chain, message):
    print(message)
    chain.stop()
    chain.purge()
    sys.exit(1)


def wait_for_stalling(chain):
    sleep(30)
    finalized_30 = check_highest(chain)
    print(f'There are {finalized_30} finalized blocks now. Waiting a little bit more.')

    sleep(10)
    finalized_40 = check_highest(chain)
    if finalized_40 != finalized_30:
        panic(chain, 'Chain is not running long enough to witness breakage.')
    print(f'There are still {finalized_40} finalized  blocks. Finalization stalled.')

    hash = chain[0].check_hash_of(finalized_40)
    if not hash:
        panic(chain, 'First node does not know hash of the highest finalized.')
    return hash


def update_chainspec(chain, hash):
    print(hash)


def test_code_substitute():
    check_build_files()

    chain = run_corrupted_binary()
    query_runtime_version(chain)
    hash = wait_for_stalling(chain)

    update_chainspec(chain, hash)


if __name__ == '__main__':
    test_code_substitute()
