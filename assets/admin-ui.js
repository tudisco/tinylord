riot.register('admin-app', {
  css: `admin-app,[is="admin-app"]{ color: #172033; font: 15px/1.45 ui-sans-serif, system-ui, sans-serif; }admin-app main,[is="admin-app"] main{ margin: 0 auto; max-width: 880px; padding: 36px 20px 64px; }admin-app header,[is="admin-app"] header,admin-app .section-head,[is="admin-app"] .section-head,admin-app li,[is="admin-app"] li,admin-app .inline-form,[is="admin-app"] .inline-form{ align-items: center; display: flex; gap: 12px; justify-content: space-between; }admin-app h1,[is="admin-app"] h1,admin-app h2,[is="admin-app"] h2,admin-app p,[is="admin-app"] p{ margin: 0; }admin-app h1,[is="admin-app"] h1{ font-size: 30px; }admin-app h2,[is="admin-app"] h2{ font-size: 18px; }admin-app .eyebrow,[is="admin-app"] .eyebrow{ color: #667085; font-size: 12px; font-weight: 700; letter-spacing: .08em; text-transform: uppercase; }admin-app article,[is="admin-app"] article{ background: #fff; border: 1px solid #d9deea; border-radius: 12px; margin-top: 16px; padding: 20px; }admin-app .token-form,[is="admin-app"] .token-form,admin-app .inline-form,[is="admin-app"] .inline-form{ display: flex; gap: 8px; }admin-app .inline-form,[is="admin-app"] .inline-form{ justify-content: flex-start; margin: 16px 0; }admin-app input,[is="admin-app"] input,admin-app select,[is="admin-app"] select{ border: 1px solid #c6cedd; border-radius: 7px; font: inherit; padding: 9px 10px; }admin-app button,[is="admin-app"] button{ background: #2457d6; border: 0; border-radius: 7px; color: white; cursor: pointer; font: inherit; padding: 9px 12px; }admin-app button:disabled,[is="admin-app"] button:disabled{ opacity: .6; }admin-app .status,[is="admin-app"] .status{ color: #b42318; margin-top: 16px; }admin-app .list,[is="admin-app"] .list{ list-style: none; margin: 0; padding: 0; }admin-app li,[is="admin-app"] li{ border-top: 1px solid #edf0f5; padding: 11px 0; }admin-app li > div,[is="admin-app"] li > div{ display: grid; }admin-app small,[is="admin-app"] small{ color: #667085; }admin-app .switch,[is="admin-app"] .switch{ display: flex; gap: 8px; align-items: center; } @media (max-width: 650px) {admin-app header,[is="admin-app"] header,admin-app .token-form,[is="admin-app"] .token-form,admin-app .inline-form,[is="admin-app"] .inline-form{ align-items: stretch; flex-direction: column; } }`,

  exports: {
    state: {
      token: '',
      connected: false,
      loading: false,
      status: '',
      databases: [],
      principals: [],
      registration: false,
    },

    setToken(event) {
      this.update({ token: event.target.value })
    },

    headers() {
      return {
        authorization: `Bearer ${this.state.token}`,
        'content-type': 'application/json',
      }
    },

    async request(path, options = {}) {
      const response = await fetch(path, { ...options, headers: { ...this.headers(), ...options.headers } })
      const payload = response.status === 204 ? null : await response.json().catch(() => null)
      if (!response.ok) throw new Error(payload?.error?.message || `Request failed (${response.status})`)
      return payload
    },

    async connect(event) {
      event.preventDefault()
      this.update({ loading: true, status: '' })
      try {
        const [databases, principals, registration] = await Promise.all([
          this.request('/v1/admin/databases'),
          this.request('/v1/admin/principals'),
          this.request('/v1/admin/auth/registration'),
        ])
        this.update({ connected: true, databases: databases.databases, principals: principals.principals, registration: registration.enabled })
      } catch (error) {
        this.update({ connected: false, status: error.message })
      } finally {
        this.update({ loading: false })
      }
    },

    async refresh() {
      const [databases, principals, registration] = await Promise.all([
        this.request('/v1/admin/databases'),
        this.request('/v1/admin/principals'),
        this.request('/v1/admin/auth/registration'),
      ])
      this.update({ databases: databases.databases, principals: principals.principals, registration: registration.enabled })
    },

    async toggleRegistration(event) {
      try {
        await this.request('/v1/admin/auth/registration', { method: 'PUT', body: JSON.stringify({ enabled: event.target.checked }) })
        this.update({ registration: event.target.checked, status: '' })
      } catch (error) {
        event.target.checked = this.state.registration
        this.update({ status: error.message })
      }
    },

    async createDatabase(event) {
      event.preventDefault()
      const name = event.target.database.value
      try {
        await this.request('/v1/admin/databases', { method: 'POST', body: JSON.stringify({ name }) })
        event.target.reset()
        await this.refresh()
      } catch (error) {
        this.update({ status: error.message })
      }
    },

    async createBrowserUser(event) {
      event.preventDefault()
      const name = event.target.username.value
      const password = event.target.password.value
      try {
        await this.request('/v1/admin/principals', { method: 'POST', body: JSON.stringify({ name, password }) })
        event.target.reset()
        await this.refresh()
      } catch (error) {
        this.update({ status: error.message })
      }
    },

    async createGrant(event) {
      event.preventDefault()
      const principal_id = event.target.principal_id.value
      const database = event.target.database.value
      const role = event.target.role.value
      try {
        await this.request('/v1/admin/grants', { method: 'POST', body: JSON.stringify({ principal_id, database, role }) })
        event.target.reset()
        await this.refresh()
      } catch (error) {
        this.update({ status: error.message })
      }
    },

    formatTime(value) { return new Date(value).toLocaleString() },
    grantLabel(grants) { return grants.length ? grants.map((grant) => `${grant.database}: ${grant.role}`).join(', ') : 'No grants' }
  },

  template: (
    template,
    expressionTypes,
    bindingTypes,
    getComponent
  ) => template(
    '<main><header><div><p class="eyebrow">TinyLord</p><h1>Admin</h1></div><form expr0="expr0" class="token-form"><input expr1="expr1" type="password" placeholder="Global admin token" autocomplete="off"/><button expr2="expr2"> </button></form></header><p expr3="expr3" class="status"></p><section expr4="expr4"></section></main>',
    [
      {
        redundantAttribute: 'expr0',
        selector: '[expr0]',

        expressions: [
          {
            type: expressionTypes.EVENT,
            name: 'onsubmit',
            evaluate: _scope => _scope.connect
          }
        ]
      },
      {
        redundantAttribute: 'expr1',
        selector: '[expr1]',

        expressions: [
          {
            type: expressionTypes.VALUE,
            evaluate: _scope => _scope.state.token
          },
          {
            type: expressionTypes.EVENT,
            name: 'oninput',
            evaluate: _scope => _scope.setToken
          }
        ]
      },
      {
        redundantAttribute: 'expr2',
        selector: '[expr2]',

        expressions: [
          {
            type: expressionTypes.TEXT,
            childNodeIndex: 0,
            evaluate: _scope => _scope.state.loading ? 'Loading…' : 'Connect'
          },
          {
            type: expressionTypes.ATTRIBUTE,
            isBoolean: true,
            name: 'disabled',
            evaluate: _scope => _scope.state.loading
          }
        ]
      },
      {
        type: bindingTypes.IF,
        evaluate: _scope => _scope.state.status,
        redundantAttribute: 'expr3',
        selector: '[expr3]',

        template: template(
          ' ',
          [
            {
              expressions: [
                {
                  type: expressionTypes.TEXT,
                  childNodeIndex: 0,
                  evaluate: _scope => _scope.state.status
                }
              ]
            }
          ]
        )
      },
      {
        type: bindingTypes.IF,
        evaluate: _scope => _scope.state.connected,
        redundantAttribute: 'expr4',
        selector: '[expr4]',

        template: template(
          '<article><div class="section-head"><h2>Registration</h2><label class="switch"><input expr5="expr5" type="checkbox"/><span>Allow public registration</span></label></div></article><article><div class="section-head"><h2>Databases</h2></div><form expr6="expr6" class="inline-form"><input name="database" placeholder="Database name" required pattern="[A-Za-z0-9_-]+"/><button>Create database</button></form><ul class="list"><li expr7="expr7"></li></ul></article><article><div class="section-head"><h2>Users & tokens</h2><small expr10="expr10"> </small></div><form expr11="expr11" class="inline-form"><input expr12="expr12" name="username" placeholder="Username" required/><input name="password" type="password" placeholder="Password (12+ characters)" required minlength="12"/><button>Create browser user</button></form><form expr13="expr13" class="inline-form"><input name="principal_id" placeholder="Principal ID" required/><input name="database" placeholder="Database" required pattern="[A-Za-z0-9_-]+"/><select name="role"><option>read</option><option>write</option><option>admin</option></select><button>Grant access</button></form><ul class="list"><li expr14="expr14"></li></ul></article>',
          [
            {
              redundantAttribute: 'expr5',
              selector: '[expr5]',

              expressions: [
                {
                  type: expressionTypes.ATTRIBUTE,
                  isBoolean: true,
                  name: 'checked',
                  evaluate: _scope => _scope.state.registration
                },
                {
                  type: expressionTypes.EVENT,
                  name: 'onchange',
                  evaluate: _scope => _scope.toggleRegistration
                }
              ]
            },
            {
              redundantAttribute: 'expr6',
              selector: '[expr6]',

              expressions: [
                {
                  type: expressionTypes.EVENT,
                  name: 'onsubmit',
                  evaluate: _scope => _scope.createDatabase
                }
              ]
            },
            {
              type: bindingTypes.EACH,
              getKey: null,
              condition: null,

              template: template(
                '<strong expr8="expr8"> </strong><small expr9="expr9"> </small>',
                [
                  {
                    redundantAttribute: 'expr8',
                    selector: '[expr8]',

                    expressions: [
                      {
                        type: expressionTypes.TEXT,
                        childNodeIndex: 0,
                        evaluate: _scope => _scope.database.name
                      }
                    ]
                  },
                  {
                    redundantAttribute: 'expr9',
                    selector: '[expr9]',

                    expressions: [
                      {
                        type: expressionTypes.TEXT,
                        childNodeIndex: 0,

                        evaluate: _scope => [
                          'Created ',
                          _scope.formatTime(
                            _scope.database.created_at
                          )
                        ].join(
                          ''
                        )
                      }
                    ]
                  }
                ]
              ),

              redundantAttribute: 'expr7',
              selector: '[expr7]',
              itemName: 'database',
              indexName: null,
              evaluate: _scope => _scope.state.databases
            },
            {
              redundantAttribute: 'expr10',
              selector: '[expr10]',

              expressions: [
                {
                  type: expressionTypes.TEXT,
                  childNodeIndex: 0,

                  evaluate: _scope => [
                    _scope.state.principals.length,
                    ' total'
                  ].join(
                    ''
                  )
                }
              ]
            },
            {
              redundantAttribute: 'expr11',
              selector: '[expr11]',

              expressions: [
                {
                  type: expressionTypes.EVENT,
                  name: 'onsubmit',
                  evaluate: _scope => _scope.createBrowserUser
                }
              ]
            },
            {
              redundantAttribute: 'expr12',
              selector: '[expr12]',

              expressions: [
                {
                  type: expressionTypes.ATTRIBUTE,
                  isBoolean: false,
                  name: 'pattern',

                  evaluate: _scope => [
                    '[A-Za-z0-9_-]',
                    (3, 64)
                  ].join(
                    ''
                  )
                }
              ]
            },
            {
              redundantAttribute: 'expr13',
              selector: '[expr13]',

              expressions: [
                {
                  type: expressionTypes.EVENT,
                  name: 'onsubmit',
                  evaluate: _scope => _scope.createGrant
                }
              ]
            },
            {
              type: bindingTypes.EACH,
              getKey: null,
              condition: null,

              template: template(
                '<div><strong expr15="expr15"> </strong><small expr16="expr16"> </small></div><small expr17="expr17"> </small>',
                [
                  {
                    redundantAttribute: 'expr15',
                    selector: '[expr15]',

                    expressions: [
                      {
                        type: expressionTypes.TEXT,
                        childNodeIndex: 0,
                        evaluate: _scope => _scope.principal.name
                      }
                    ]
                  },
                  {
                    redundantAttribute: 'expr16',
                    selector: '[expr16]',

                    expressions: [
                      {
                        type: expressionTypes.TEXT,
                        childNodeIndex: 0,

                        evaluate: _scope => [
                          _scope.principal.id,
                          ' · ',
                          _scope.principal.kind,
                          _scope.principal.is_admin ? ' · global admin' : '',
                          _scope.principal.disabled ? ' · disabled' : ''
                        ].join(
                          ''
                        )
                      }
                    ]
                  },
                  {
                    redundantAttribute: 'expr17',
                    selector: '[expr17]',

                    expressions: [
                      {
                        type: expressionTypes.TEXT,
                        childNodeIndex: 0,

                        evaluate: _scope => _scope.grantLabel(
                          _scope.principal.grants
                        )
                      }
                    ]
                  }
                ]
              ),

              redundantAttribute: 'expr14',
              selector: '[expr14]',
              itemName: 'principal',
              indexName: null,
              evaluate: _scope => _scope.state.principals
            }
          ]
        )
      }
    ]
  ),

  name: 'admin-app'
});
