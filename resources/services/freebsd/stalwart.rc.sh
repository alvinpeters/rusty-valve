#!/bin/sh
#
#

# PROVIDE: stalwart
# REQUIRE: DAEMON
# KEYWORD: shutdown

. /etc/rc.subr

name=stalwart
_binname=${name}-mail
rcvar=${name}_enable 

config_file="/usr/local/etc/${name}/config.toml"

load_rc_config $name
: ${stalwart_enable:="NO"}

pidfile="/var/run/${name}.pid"
procname="/usr/local/bin/${_binname}"
procargs="--config ${config_file}"

command="/usr/sbin/daemon"
command_args="-S -p ${pidfile} ${procname} ${procargs}"

run_rc_command "$1"
