import os
import logging
import webbrowser
from argparse import Namespace
from time import sleep
from typing import List, Optional

import yaml
from shell import setup_benchmark, instances_ip_in_region, run_task
from utils import default_region

from .utils import copy_binary


def run_experiment(nparties: int, tag: str, unit_creation_delay: Optional[int]) -> List[str]:
    logging.info('Setting up nodes...')
    flags = {'--unit-creation-delay': unit_creation_delay} if unit_creation_delay else dict()
    setup_benchmark(nparties, 'test', [default_region()], tag=tag, node_flags=flags)
    logging.info('Obtaining machine IPs...')
    ips = instances_ip_in_region(tag=tag)
    logging.info(f'Machine IPs: {ips}.')
    logging.info('Dispatching the task...')
    run_task('dispatch', regions=[default_region()], tag=tag)

    logging.info('Running experiment succeeded.')
    return ips


def convert_to_targets(ips: List[str]) -> List[str]:
    return [f'{ip}:9615' for ip in ips]


def create_prometheus_configuration(targets: List[str]):
    logging.info('Creating Prometheus configuration...')

    config = {'scrape_configs': [{
        'job_name': 'aleph-nodes',
        'scrape_interval': '5s',
        'static_configs': [{'targets': targets}]
    }]}

    with open('prometheus.yml', 'w') as yml_file:
        yaml.dump(config, yml_file)

    logging.info('Prometheus configuration saved to `prometheus.yml`.')


def run_monitoring_in_docker():
    os.system('docker-compose up -d')


def view_dashboard():
    sleep(2.)  # sometimes the browser is open before Grafana server is up
    webbrowser.open('http://localhost:3000/', 2)


def run(args: Namespace):
    copy_binary(args.aleph_node_binary, 'aleph-node')
    ips = run_experiment(args.nparties, args.tag, args.unit_creation_delay)
    targets = convert_to_targets(ips)
    create_prometheus_configuration(targets)
    run_monitoring_in_docker()
    view_dashboard()