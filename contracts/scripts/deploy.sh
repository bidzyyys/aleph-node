#!/bin/bash

set -euo pipefail

# --- FUNCTIONS

function instrument_game_token {

  local  __resultvar=$1
  local contract_name=$2
  local salt=$3

  # --- CREATE AN INSTANCE OF THE TOKEN CONTRACT

  cd $CONTRACTS_PATH/$contract_name

  CONTRACT_ADDRESS=$(cargo contract instantiate --url $NODE --constructor new --args $GAME_BALANCE --suri $AUTHORITY_SEED --salt $salt)
  CONTRACT_ADDRESS=$(echo "$CONTRACT_ADDRESS" | grep Contract | tail -1 | cut -c 15-)

  echo $contract_name "token contract instance address: " $CONTRACT_ADDRESS

  # --- GRANT PRIVILEDGES ON THE TOKEN CONTRACT

  cd $CONTRACTS_PATH/access_control

  # set the admin and the owner of the contract instance
  cargo contract call --url $NODE --contract $ACCESS_CONTROL --message grant_role --args $AUTHORITY 'Admin('$CONTRACT_ADDRESS')' --suri $AUTHORITY_SEED
  cargo contract call --url $NODE --contract $ACCESS_CONTROL --message grant_role --args $AUTHORITY 'Owner('$CONTRACT_ADDRESS')' --suri $AUTHORITY_SEED

  eval $__resultvar="'$CONTRACT_ADDRESS'"
}

function deploy_and_instrument_game {

  local  __resultvar=$1
  local contract_name=$2
  local game_token=$3

  # --- UPLOAD CONTRACT CODE

  cd $CONTRACTS_PATH/$contract_name
  link_bytecode $contract_name 4465614444656144446561444465614444656144446561444465614444656144 $ACCESS_CONTROL_PUBKEY
  rm target/ink/$contract_name.wasm
  node ../scripts/hex-to-wasm.js target/ink/$contract_name.contract target/ink/$contract_name.wasm

  CODE_HASH=$(cargo contract upload --url $NODE --suri $AUTHORITY_SEED)
  CODE_HASH=$(echo "$CODE_HASH" | grep hash | tail -1 | cut -c 15-)

  # --- GRANT INIT PRIVILEDGES ON THE CONTRACT CODE

  cd $CONTRACTS_PATH/access_control

  cargo contract call --url $NODE --contract $ACCESS_CONTROL --message grant_role --args $AUTHORITY 'Initializer('$CODE_HASH')' --suri $AUTHORITY_SEED

  # --- CREATE AN INSTANCE OF THE CONTRACT

  cd $CONTRACTS_PATH/$contract_name

  CONTRACT_ADDRESS=$(cargo contract instantiate --url $NODE --constructor new --args $game_token $LIFETIME --suri $AUTHORITY_SEED)
  CONTRACT_ADDRESS=$(echo "$CONTRACT_ADDRESS" | grep Contract | tail -1 | cut -c 15-)

  echo $contract_name "contract instance address: " $CONTRACT_ADDRESS

  # --- GRANT PRIVILEDGES ON THE CONTRACT

  cd $CONTRACTS_PATH/access_control

  cargo contract call --url $NODE --contract $ACCESS_CONTROL --message grant_role --args $AUTHORITY 'Owner('$CONTRACT_ADDRESS')' --suri $AUTHORITY_SEED
  cargo contract call --url $NODE --contract $ACCESS_CONTROL --message grant_role --args $AUTHORITY 'Admin('$CONTRACT_ADDRESS')' --suri $AUTHORITY_SEED

  # --- TRANSFER TOKENS TO THE CONTRACT

  cd $CONTRACTS_PATH/button_token

  cargo contract call --url $NODE --contract $game_token --message transfer --args $CONTRACT_ADDRESS $GAME_BALANCE --suri $AUTHORITY_SEED

  # --- WHITELIST ACCOUNTS FOR PLAYING

  cd $CONTRACTS_PATH/$contract_name

  cargo contract call --url $NODE --contract $CONTRACT_ADDRESS --message IButtonGame::bulk_allow --args $WHITELIST --suri $AUTHORITY_SEED

  eval $__resultvar="'$CONTRACT_ADDRESS'"
}

function link_bytecode() {
  local CONTRACT=$1
  local PLACEHOLDER=$2
  local REPLACEMENT=$3

  sed -i 's/'$PLACEHOLDER'/'$REPLACEMENT'/' target/ink/$CONTRACT.contract
}

# --- GLOBAL CONSTANTS

# TODO : configurable ARGS (source env/dev)
# TODO : split to deploy and test part

NODE_IMAGE=public.ecr.aws/p6e8q1z1/aleph-node:latest

# NODE=ws://127.0.0.1:9943

# AUTHORITY=5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY
# AUTHORITY_SEED=//Alice

# NODE0=5D34dL5prEUaGNQtPPZ3yN5Y6BnkfXunKXXz6fo7ZJbLwRRH
# NODE0_SEED=//0

# LIFETIME=5
# mint this many game tokens
GAME_BALANCE=1000

CONTRACTS_PATH=$(pwd)/contracts

# --- COMPILE CONTRACTS

cd $CONTRACTS_PATH/access_control
cargo contract build --release

cd $CONTRACTS_PATH/button_token
cargo contract build --release

cd $CONTRACTS_PATH/early_bird_special
cargo contract build --release

cd $CONTRACTS_PATH/back_to_the_future
cargo contract build --release

# --- DEPLOY ACCESS CONTROL CONTRACT

cd $CONTRACTS_PATH/access_control

CONTRACT=$(cargo contract instantiate --url $NODE --constructor new --suri $AUTHORITY_SEED)
ACCESS_CONTROL=$(echo "$CONTRACT" | grep Contract | tail -1 | cut -c 15-)
ACCESS_CONTROL_PUBKEY=$(docker run --rm --entrypoint "/bin/sh" "${NODE_IMAGE}" -c "aleph-node key inspect $ACCESS_CONTROL" | grep hex | cut -c 23- | cut -c 3-)

echo "access control contract address: " $ACCESS_CONTROL
echo "access control contract public key (hex): " $ACCESS_CONTROL_PUBKEY

# --- UPLOAD TOKEN CONTRACT CODE

cd $CONTRACTS_PATH/button_token
# replace address placeholder with the on-chain address of the AccessControl contract
link_bytecode button_token 4465614444656144446561444465614444656144446561444465614444656144 $ACCESS_CONTROL_PUBKEY
# remove just in case
rm target/ink/button_token.wasm
# NOTE : here we go from hex to binary using a nodejs cli tool
# availiable from https://github.com/fbielejec/polkadot-cljs
node ../scripts/hex-to-wasm.js target/ink/button_token.contract target/ink/button_token.wasm

CODE_HASH=$(cargo contract upload --url $NODE --suri $AUTHORITY_SEED)
BUTTON_TOKEN_CODE_HASH=$(echo "$CODE_HASH" | grep hash | tail -1 | cut -c 15-)

echo "button token code hash" $BUTTON_TOKEN_CODE_HASH

# --- GRANT INIT PRIVILEDGES ON THE TOKEN CONTRACT CODE

cd $CONTRACTS_PATH/access_control

# set the initializer of the button-token contract
cargo contract call --url $NODE --contract $ACCESS_CONTROL --message grant_role --args $AUTHORITY 'Initializer('$BUTTON_TOKEN_CODE_HASH')' --suri $AUTHORITY_SEED

#
# --- EARLY_BIRD_SPECIAL GAME
#

# --- CREATE AN INSTANCE OF THE TOKEN CONTRACT FOR THE EARLY_BIRD_SPECIAL GAME

instrument_game_token EARLY_BIRD_SPECIAL_TOKEN button_token 0x4561726C79426972645370656369616C

# --- UPLOAD CODE AND CREATE AN INSTANCE OF THE EARLY_BIRD_SPECIAL GAME CONTRACT

deploy_and_instrument_game EARLY_BIRD_SPECIAL early_bird_special $EARLY_BIRD_SPECIAL_TOKEN

#
# --- BACK_TO_THE_FUTURE GAME
#

# --- CREATE AN INSTANCE OF THE TOKEN CONTRACT FOR THE BACK_TO_THE_FUTURE GAME

instrument_game_token BACK_TO_THE_FUTURE_TOKEN button_token 0x4261636B546F546865467574757265

# --- UPLOAD CODE AND CREATE AN INSTANCE OF THE EARLY_BIRD_SPECIAL GAME CONTRACT

deploy_and_instrument_game BACK_TO_THE_FUTURE back_to_the_future $BACK_TO_THE_FUTURE_TOKEN

# spit adresses to a JSON file
cd $CONTRACTS_PATH

jq -n --arg early_bird_special $EARLY_BIRD_SPECIAL \
   --arg back_to_the_future $BACK_TO_THE_FUTURE \
   '{early_bird_special: $early_bird_special, back_to_the_future: $back_to_the_future}' > addresses.json
