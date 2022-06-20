#!/bin/env python
import os
import sys
from os.path import abspath, join
from time import sleep

from chainrunner import Chain, Seq, generate_keys, check_finalized

# Path to working directory, where chainspec, logs and nodes' dbs are written:
workdir = abspath(os.getenv('WORKDIR', '/tmp/workdir'))
# Path to the aleph-node binary (important DON'T use short-session feature):
binary = abspath(os.getenv('ALEPH_NODE_BINARY', join(workdir, 'aleph-node')))

phrases = [f'//{i}' for i in range(6)]
keys = generate_keys(binary, phrases)
all_accounts = list(keys.values())
chain = Chain(workdir)
print('Bootstrapping the chain with binary')
chain.bootstrap(binary,
                all_accounts[:4],
                nonvalidators=all_accounts[4:],
                sudo_account_id=keys[phrases[0]],
                chain_type='local')

chain.set_flags(port=Seq(30334),
                ws_port=Seq(9944),
                rpc_port=Seq(9933),
                unit_creation_delay=200,
                execution='Native')

chain.set_flags_validator('validator')

# AlephBFT needs to know the address up front, otherwise it waits for a while
# and retries; and while the retry does succeed, that's time we don't want to
# take.
for node in chain.nodes:
	port = node.flags['port']
	node.flags['public-addr'] = f'/ip4/127.0.0.1/tcp/{port}'

print('Starting the chain')
chain.start('aleph')

for run_duration, stop_duration, catch_up_duration in [[90, 30, 45], [5, 10, 20], [5, 10, 20]]:
	print(f'Waiting {run_duration}s')
	sleep(run_duration)

	finalized_before_kill_per_node = check_finalized(chain)
	print('Killing one validator and one nonvalidator')

	chain[3].stop()
	chain[4].stop()

	print(f'Waiting {stop_duration}s')
	sleep(stop_duration)

	print('Restarting nodes')
	finalized_before_start_per_node = check_finalized(chain)

	# Check if the finalization didn't stop after a kill.
	# Use a reduced rate since 1/4 nodes are offline, and fudge by 0.9 to give a margin of error.
	if finalized_before_start_per_node[0] - finalized_before_kill_per_node[0] < stop_duration * (3/4) * 0.9:
		print('Finalization stalled')
		sys.exit(1)

	chain.start('aleph', nodes=[3, 4])

	print(f'Waiting {catch_up_duration}s for catch up')
	sleep(catch_up_duration)
	finalized_after_catch_up_per_node = check_finalized(chain)

	nonvalidator_diff = finalized_after_catch_up_per_node[4] - finalized_before_start_per_node[4]
	validator_diff = finalized_after_catch_up_per_node[3] - finalized_before_start_per_node[3]

	ALLOWED_DELTA = 5

	# Checks if the murdered nodes started catching up with reasonable nr of blocks.
	if nonvalidator_diff <= ALLOWED_DELTA:
		print(f"too small catch up for nonvalidators: {nonvalidator_diff}")
		sys.exit(1)

	if validator_diff <= ALLOWED_DELTA:
		print(f"too small catch up for validators: {validator_diff}")
		sys.exit(1)