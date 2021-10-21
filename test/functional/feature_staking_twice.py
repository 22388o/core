#!/usr/bin/env python3
# Copyright (c) 2017 The Bitcoin Core developers
# Distributed under the MIT software license, see the accompanying
# file COPYING or http://www.opensource.org/licenses/mit-license.php.
"""An example functional test

Send a transaction from Alice to Bob, then Bob stakes for the first time.. and an extra one.
"""

from substrateinterface import Keypair
import test_framework.mintlayer.utxo as utxo

from test_framework.test_framework import MintlayerTestFramework
from test_framework.util import (
    assert_equal,
    connect_nodes,
    wait_until,
)
from test_framework.messages import COIN


class ExampleTest(MintlayerTestFramework):
    # Each functional test is a subclass of the MintlayerTestFramework class.

    # Override the set_test_params(), add_options(), setup_chain(), setup_network()
    # and setup_nodes() methods to customize the test setup as required.

    def set_test_params(self):
        """Override test parameters for your individual test.

        This method must be overridden and num_nodes must be exlicitly set."""
        self.setup_clean_chain = True
        self.num_nodes = 1
        # Use self.extra_args to change command-line arguments for the nodes
        self.extra_args = [[]]

        # self.log.info("I've finished set_test_params")  # Oops! Can't run self.log before run_test()

    def setup_network(self):
        """Setup the test network topology

        Often you won't need to override this, since the standard network topology
        (linear: node0 <-> node1 <-> node2 <-> ...) is fine for most tests.

        If you do override this method, remember to start the nodes, assign
        them to self.nodes, connect them and then sync."""

        self.setup_nodes()

    def custom_method(self):
        """Do some custom behaviour for this test

        Define it in a method here because you're going to use it repeatedly.
        If you think it's useful in general, consider moving it to the base
        MintlayerTestFramework class so other tests can use it."""

        self.log.info("Running custom_method")

    def run_test(self):
        client = self.nodes[0].rpc_client

        alice = Keypair.create_from_uri('//Alice')
        bob = Keypair.create_from_uri('//Bob')
        bob_stash = Keypair.create_from_uri('//Bob//stash')

        # fetch the genesis utxo from storage
        utxos = list(client.utxos_for(alice))

        # there's only 1 record of staking, which is alice.
        assert_equal( len(list(client.staking_count())), 1 )

        tx1 = utxo.Transaction(
            client,
            inputs=[
                utxo.Input(utxos[0][0]),
            ],
            outputs=[
                utxo.Output(
                    value=70000 * COIN,
                    header=0,
                    destination=utxo.DestPubkey(bob.public_key)
                ),
            ]
        ).sign(alice, [utxos[0][1]])
        client.submit(alice, tx1)

        tx2 = utxo.Transaction(
            client,
            inputs=[
                utxo.Input(tx1.outpoint(0)),
            ],
            outputs=[
                utxo.Output(
                    value=40000 * COIN,
                    header=0,
                    destination=utxo.DestStake(bob_stash.public_key, bob.public_key,'0xa03bcfaac6ebdc26bb9c256c51b08f9c1c6d4569f48710a42939168d1d7e5b6086b20e145e97158f6a0b5bff2994439d3320543c8ff382d1ab3e5eafffaf1a18')
                ),
                 utxo.Output(
                    value=10000 * COIN,
                    header=0,
                    destination=utxo.DestStakeExtra(alice.public_key)
                ),
                utxo.Output(
                    value=19998 * COIN,
                    header=0,
                    destination=utxo.DestPubkey(bob.public_key)
                ),
            ]
        ).sign(bob, tx1.outputs)

        client.submit(bob, tx2)

        staking_count = list(client.staking_count())
        # there should already be 2 accounts, adding Bob in the list.
        assert_equal(len(staking_count), 2)

        # bob should have 2 locked utxos
        assert_equal(staking_count[0][1][0], 2)

        # bob should have a total of 50000 * COINS locked
        assert_equal(staking_count[0][1][1], 50000 * COIN)

        # fetch the locked utxos from storage
        locked_utxos = list(client.utxos('LockedUtxos'))
        # there should already be 3 in the list; 1 from alice, 2 from bob
        assert_equal(len(locked_utxos),3)


if __name__ == '__main__':
    ExampleTest().main()
