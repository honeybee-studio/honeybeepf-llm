#!/bin/bash#!/bin/bash

set -eset -e

INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

CONFIG_DIR="${CONFIG_DIR:-/etc/honeybeepf}"CONFIG_DIR="${CONFIG_DIR:-/etc/honeybeepf}"

echo "Installing honeybeepf..."echo "Installing honeybeepf..."

sudo install -m 755 honeybeepf "$INSTALL_DIR/honeybeepf"sudo install -m 755 honeybeepf "$INSTALL_DIR/honeybeepf"

sudo mkdir -p "$CONFIG_DIR"sudo mkdir -p "$CONFIG_DIR"

if [ -f honeybeepf.env.example ]; thenif [ -f honeybeepf.env.example ]; then

    sudo cp honeybeepf.env.example "$CONFIG_DIR/"    sudo cp honeybeepf.env.example "$CONFIG_DIR/"

    if [ ! -f "$CONFIG_DIR/honeybeepf.env" ]; then    if [ ! -f "$CONFIG_DIR/honeybeepf.env" ]; then

        sudo cp honeybeepf.env.example "$CONFIG_DIR/honeybeepf.env"        sudo cp honeybeepf.env.example "$CONFIG_DIR/honeybeepf.env"

    fi    fi

fifi

echo "✅ Installed to $INSTALL_DIR/honeybeepf"echo "✅ Installed to $INSTALL_DIR/honeybeepf"

